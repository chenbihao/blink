/**
 * 插件 Tab 模块
 * 包含：内置动作列表、插件管理（schema 渲染 + 触发词 + 拖拽排序）
 *
 * 插件 schema 渲染主体搬自原 settings.js 797–1560（0.9.5 拆分时被残缺重写，
 * 命令名 list_plugins 与容器 plugins-list 均不存在导致页面空白；0.9.5.1 还原原版
 * get_plugins + plugins-container + 整套 schema/触发词/拖拽）。
 */
import {commandErrorText, confirmDialog, invoke, listen, openDialog} from "../../shared/tauri.js";
import {onLangChange, t} from "../../i18n/index.js";
import {iconHTML} from "../../shared/icon.js";
import {saveConfig} from "../../shared/config-keys.js";
import {navigateSettings} from "../navigation.js";
import {renderWeatherSettings, mountWeatherSettings, WEATHER_EDITOR_CLOSED} from "./weather-settings.js";
import {bindCredentialFields, refreshCredentialGroups} from "./plugin-credentials.js";
import {EVENTS} from "../../shared/event-names.js";
let pluginsLoadRevision = 0;
let pluginWrites = 0;
const weatherEditors = new Map();
let pluginConfigListener = false;
let pendingPluginReload = false;
const renderedPlugins = new Map();
// 0.28.2：插件窗口面状态（get_plugin_surface_status → pages/readiness/install），
// 与 get_plugins 同轮拉取；拉取失败静默降级（无窗口行的旧展示）。
const surfaceStatus = new Map();
// 0.28.5：官方插件中心目录状态（get_plugin_catalog）。null/加载中/错误态都
// 不影响已安装插件展示——目录区独立渲染，失败只降级"获取"视图。
let catalogState = null;
let catalogRevision = 0;
let catalogProgressBound = false;
// 进行中的目录安装（progressId → 无）：done/failed 时按 data-progress-id
// 重查 DOM（loadPlugins 重渲染会重建按钮，不持引用）。
const catalogPending = new Set();

/** 插件图标映射（0.10.8：emoji → Lucide 图标名） */
const PLUGIN_ICONS = {
    "builtin.ip": "globe",
    "builtin.ai": "sparkles",
    "builtin.translate": "languages",
    "builtin.weather": "cloud-sun",
    "builtin.dictionary": "book-open",
    // 0.25.20 示例脚本插件（终端语义）
    "example.echo-python": "terminal",
    "example.echo-node": "terminal",
};

/** 防止重复注册 onLangChange */
let _langChangeRegistered = false;

/**
 * 初始化插件 Tab
 */
export function initPluginsTab() {
    loadPlugins(true);
    loadCatalog(true);
    bindCatalogProgress();
    initNumberSpinner();
    initExternalLinkDelegate();
    if (!pluginConfigListener) {
        pluginConfigListener = true;
        document.getElementById("plugins-container")?.addEventListener("focusout", () => {
            // Wait until focus has reached its new target. loadPlugins still protects
            // an active editor, revision conflict, or queued config write.
            setTimeout(() => { if (pendingPluginReload) void loadPlugins(); }, 0);
        });
        // Settings windows are reused after hiding; reset only credential disclosure,
        // keeping the card, saved values and any editor draft in place.
        window.addEventListener("blur", () => {
            document.querySelectorAll("#plugins-container .plugin-group[open]").forEach(group => { group.open = false; });
        });
        listen(EVENTS.CONFIG_CHANGED, ({payload}) => {
            if (!payload?.key || payload.key === "plugin_config" || payload.key.startsWith("plugin:")) loadPlugins();
        });
    }
    if (!_langChangeRegistered) {
        _langChangeRegistered=true;
        onLangChange(()=>{loadPlugins(true); renderCatalog();});
    }
}
// ── 插件列表（搬自原 settings.js）─────────────────────────────────────────────

/**
 * 加载插件列表
 */
async function loadPlugins(force = false) {
    const revision = ++pluginsLoadRevision;
    const container = document.getElementById("plugins-container");
    if (!container) return;
    let plugins;
    try {
        // 0.28.2：窗口面状态并行拉取；独立容错——失败只少渲染窗口行，不阻断列表
        const [list, surface] = await Promise.all([
            invoke("get_plugins"),
            invoke("get_plugin_surface_status").catch(() => null),
        ]);
        plugins = list;
        surfaceStatus.clear();
        if (Array.isArray(surface)) {
            for (const row of surface) surfaceStatus.set(row.plugin_id, row);
        }
    } catch (error) {
        if (revision !== pluginsLoadRevision) return;
        console.error("loadPlugins failed:", error);
        if (!force && pluginWrites) {
            pendingPluginReload = true;
            return;
        }
        if (container.querySelector('[data-weather-editor][aria-hidden="false"]')) {
            pendingPluginReload = true;
            const card = container.querySelector('.extension-card[data-plugin-id="builtin.weather"]');
            if (card) flash(card, t("config.load_failed", {err: error?.message || String(error)}), true, 6000);
            return;
        }
        for (const editor of weatherEditors.values()) editor.dispose();
        weatherEditors.clear();
        container.innerHTML = `<p class="msg-error">${t("plugin.load_failed")}</p>`;
        return;
    }
    if (revision !== pluginsLoadRevision) return;
    if (!force && pluginWrites) {
        pendingPluginReload = true;
        return;
    }
    if (!Array.isArray(plugins)) plugins = [];
    if (!force && renderedPlugins.size === plugins.length && plugins.every(plugin =>
        renderedPlugins.get(plugin.id)?.revision === plugin.revision)) {
        pendingPluginReload = false;
        return;
    }
    if (!force && container.querySelector('[data-config-conflict="true"]')) {
        pendingPluginReload = true;
        return;
    }
    if (!force && renderedPlugins.size && container.contains?.(document.activeElement)) {
        pendingPluginReload = true;
        return;
    }

    if (!force && container.querySelector('[data-weather-editor][aria-hidden="false"]')) {
        pendingPluginReload = true;
        const current = renderedPlugins.get("builtin.weather");
        const latest = plugins.find(plugin => plugin.id === "builtin.weather");
        const card = container.querySelector('.extension-card[data-plugin-id="builtin.weather"]');
        if (card && latest?.revision !== current?.revision) flash(card, t("weather.config_changed"), true, 6000);
        return;
    }
    pendingPluginReload = false;
    for (const editor of weatherEditors.values()) editor.dispose();
    weatherEditors.clear();
    renderedPlugins.clear();
    for (const plugin of plugins) renderedPlugins.set(plugin.id, plugin);
    if (plugins.length === 0) {
        container.innerHTML = `<p class="msg-muted" style="padding: 20px;">${t("plugin.empty")}</p>`;
        return;
    }
    container.innerHTML = plugins.map(renderPluginCard).join("");
    for (const plugin of plugins) bindPluginCardEvents(plugin);
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

    const surfaceSection = renderSurfaceSection(plugin);
    const datasetsSection = renderDatasetsSection(
        plugin,
        surfaceStatus.get(plugin.id),
        catalogState?.catalog,
        catalogState?.devMode !== false,
    );

    const configSection = hasFields
        ? renderConfigSection(t("plugin.section"), schema, settings, {collapsible: true, collapsed: true, extraHtml: plugin.id === "builtin.weather" ? renderWeatherSettings(plugin) : "", saveLabel: ""})
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
        body: `${surfaceSection}${datasetsSection}${configSection}`,
    });
}

/**
 * 插件窗口行（0.28.2）：声明了 pages 的插件渲染「打开窗口」按钮 + 就绪状态。
 *
 * 就绪状态只做提示不做硬门禁——未就绪仍可打开（如翻译缺密钥时页面能显示
 * 修复引导）；缺失原因（blocker.hint）由后端按当前语言本地化。点击未就绪
 * 徽标展开配置区，形成「缺什么 → 去哪补」的修复入口。
 */
function renderSurfaceSection(plugin) {
    const surface = surfaceStatus.get(plugin.id);
    const pages = surface?.pages || [];
    if (!pages.length) return "";
    const readiness = surface?.readiness || {};
    const ready = readiness.ready !== false;
    const blocker = (readiness.blockers || [])[0];
    const statusHtml = ready
        ? `<span class="plugin-surface-status plugin-surface-ready">${t("plugin.surface_ready")}</span>`
        : `<button class="plugin-surface-status plugin-surface-blocked" title="${escapeHtml(blocker?.hint || "")}">${t("plugin.surface_not_ready")}</button>`;
    const buttons = pages.map(page =>
        `<button class="btn-small plugin-open-window" data-page="${escapeAttr(page.id)}">${iconHTML("app-window")} ${t("plugin.surface_open")}</button>`).join("");
    return `<div class="plugin-surface-row">
      <span class="trigger-label">${t("plugin.surface_section")}</span>
      ${statusHtml}
      <span class="plugin-surface-buttons">${buttons}</span>
    </div>`;
}

// ── 官方插件中心（0.28.5）────────────────────────────────────────────────────

/**
 * 点分数字版本比较（与后端 compare_versions 同语义：非数字段按 0，段数短的补 0）。
 * 返回 <0 / 0 / >0。导出供测试与"已最新"判定共用。
 */
export function compareVersions(a, b) {
    const parse = (s) => String(s ?? "").split(".").map((seg) => {
        const n = parseInt(seg, 10);
        return Number.isFinite(n) ? n : 0;
    });
    const va = parse(a), vb = parse(b);
    const n = Math.max(va.length, vb.length);
    for (let i = 0; i < n; i++) {
        const x = va[i] || 0, y = vb[i] || 0;
        if (x !== y) return x - y;
    }
    return 0;
}

/**
 * 拉取官方目录（独立于插件列表：失败只降级目录区，不阻断已安装展示）。
 */
async function loadCatalog(force = false) {
    const revision = ++catalogRevision;
    const container = document.getElementById("plugin-catalog-container");
    if (!container) return;
    container.innerHTML = `<p class="msg-muted plugin-catalog-loading">${t("plugin.catalog_loading")}</p>`;
    try {
        const res = await invoke("get_plugin_catalog", {forceRefresh: force});
        if (revision !== catalogRevision) return;
        catalogState = res;
    } catch (error) {
        if (revision !== catalogRevision) return;
        catalogState = {catalog: null, stale: false, devMode: true, error: commandErrorText(error, t("plugin.catalog_unavailable"))};
    }
    if (revision === catalogRevision) renderCatalog();
}

function renderCatalog() {
    const container = document.getElementById("plugin-catalog-container");
    if (!container) return;
    container.innerHTML = renderCatalogSection(catalogState, surfaceStatus);
    bindCatalogEvents(container);
}

/**
 * 渲染插件中心目录区（0.28.5）。导出为纯字符串函数供测试：
 * - state 为 get_plugin_catalog 的返回（catalog/stale/devMode）或 {error} 错误态；
 * - surfaceStatus 为 plugin_id → get_plugin_surface_status 行的 Map；
 *   行的 install.version（用户目录安装）或 version（随包 manifest）用于
 *   "已最新 / 可更新"判定。
 */
export function renderCatalogSection(state, surfaceStatusMap) {
    if (!state) return `<p class="msg-muted plugin-catalog-loading">${t("plugin.catalog_loading")}</p>`;
    if (state.error || !state.catalog) {
        return `<div class="plugin-catalog plugin-catalog-error">
          <span class="msg-error">${escapeHtml(state.error || t("plugin.catalog_unavailable"))}</span>
          <button class="btn-small plugin-catalog-refresh">${iconHTML("refresh-cw")} ${t("plugin.catalog_retry")}</button>
        </div>`;
    }
    const rows = (state.catalog.plugins || []).map((entry) => {
        const surface = surfaceStatusMap?.get?.(entry.plugin_id);
        const current = surface?.install?.version || surface?.version;
        const upToDate = current != null && compareVersions(entry.version, current) <= 0;
        const action = upToDate
            ? `<span class="plugin-catalog-status plugin-catalog-uptodate">${t("plugin.catalog_uptodate")}</span>`
            : `<button class="btn-small plugin-catalog-install" data-plugin-id="${escapeAttr(entry.plugin_id)}" data-progress-id="${escapeAttr(entry.plugin_id)}">
                 ${iconHTML("download")} ${current ? t("plugin.catalog_update") : t("plugin.catalog_install")}
               </button>`;
        const size = formatBytes(entry.asset?.bytes);
        return `<div class="plugin-catalog-row" data-plugin-id="${escapeAttr(entry.plugin_id)}">
          <span class="plugin-catalog-name">${escapeHtml(entry.plugin_id)}</span>
          <span class="version-badge">v${escapeHtml(entry.version)}</span>
          <span class="plugin-catalog-size">${escapeHtml(size)}</span>
          ${action}
        </div>`;
    }).join("");
    return `<div class="plugin-catalog">
      <div class="plugin-catalog-head">
        <span class="trigger-label">${t("plugin.catalog_section")}</span>
        ${state.stale ? `<span class="plugin-catalog-stale">${t("plugin.catalog_stale")}</span>` : ""}
        <button class="btn-small plugin-catalog-refresh">${iconHTML("refresh-cw")} ${t("plugin.catalog_refresh")}</button>
      </div>
      ${rows || `<p class="msg-muted">${t("plugin.catalog_empty")}</p>`}
    </div>`;
}

/** 字节数 → 人类可读（MB/KB），目录条目大小展示。 */
function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes <= 0) return "";
    if (bytes >= 1024 * 1024) return (bytes / 1024 / 1024).toFixed(1) + " MB";
    return Math.max(1, Math.round(bytes / 1024)) + " KB";
}

function bindCatalogEvents(container) {
    container.querySelectorAll(".plugin-catalog-refresh").forEach((btn) =>
        btn.addEventListener("click", () => loadCatalog(true)));
    container.querySelectorAll(".plugin-catalog-install").forEach((btn) =>
        btn.addEventListener("click", () => installFromCatalog(btn, "plugin", btn.dataset.pluginId)));
}

/** 目录安装入口（插件/数据包共用）：按钮置忙 → invoke → done/failed 由进度事件恢复。 */
async function installFromCatalog(button, kind, payload, progressId) {
    if (catalogPending.has(progressId)) return;
    catalogPending.add(progressId);
    button.disabled = true;
    button._label = button.innerHTML;
    button.innerHTML = t("plugin.catalog_downloading");
    try {
        await invoke(kind === "plugin" ? "install_plugin_from_catalog" : "install_dataset_from_catalog", payload);
        // done 事件负责恢复按钮与刷新；此处兜底（事件丢失时仍解锁）
        finishCatalogInstall(progressId, null);
    } catch (error) {
        finishCatalogInstall(progressId, commandErrorText(error, t("plugin.catalog_install_failed")));
    }
}

/** 安装结束：恢复按钮 + 刷新列表/目录（错误非空时提示）。 */
function finishCatalogInstall(progressId, error) {
    catalogPending.delete(progressId);
    const button = document.querySelector(`[data-progress-id="${CSS.escape(progressId)}"]`);
    if (button) {
        button.disabled = false;
        if (button._label) button.innerHTML = button._label;
    }
    if (error) {
        const host = button?.closest(".extension-card, .plugin-catalog");
        if (host) flash(host, error, true, 6000);
    }
    loadPlugins(true);
    renderCatalog();
}

/** 进度事件 → 更新进行中按钮文案（percent / 安装中 / 完成恢复）。 */
function bindCatalogProgress() {
    if (catalogProgressBound) return;
    catalogProgressBound = true;
    listen(EVENTS.PLUGIN_CENTER_PROGRESS, ({payload}) => {
        const p = payload || {};
        const button = document.querySelector(`[data-progress-id="${CSS.escape(p.id || "")}"]`);
        if (p.phase === "downloading" && button && !button.disabled) return;
        if (p.phase === "downloading" && button) {
            const pct = p.total > 0 ? Math.floor((p.received / p.total) * 100) : null;
            button.innerHTML = pct != null
                ? `${t("plugin.catalog_downloading")} ${pct}%`
                : t("plugin.catalog_downloading");
        } else if (p.phase === "installing" && button) {
            button.innerHTML = t("plugin.catalog_installing");
        } else if (p.phase === "done") {
            finishCatalogInstall(p.id, null);
        } else if (p.phase === "failed") {
            finishCatalogInstall(p.id, p.error || t("plugin.catalog_install_failed"));
        }
    });
}


/**
 * 数据包（词库）区（0.28.4）：manifest 声明 datasets 的插件逐行渲染
 * 安装状态 + 安装/更新/卸载。损坏/版本过低的判断来自 readiness
 * blockers（kind=dataset_corrupt/dataset_outdated 的 field 命中该数据包）。
 *
 * 0.28.5：官方目录有该数据包条目时主安装按钮走目录下载（data-catalog）；
 * dev 模式（默认 true，测试两参调用兼容）追加"本地 zip"按钮；生产模式且
 * 目录无条目时提示目录不可用（官方词库必在目录，此为降级路径）。
 *
 * 导出供测试（纯字符串渲染，无 DOM 依赖）；surface 参数为
 * get_plugin_surface_status 的对应行（缺省不渲染）。
 */
export function renderDatasetsSection(plugin, surface, catalog = null, devMode = true) {
    const datasets = surface?.datasets || [];
    if (!datasets.length) return "";
    const readiness = surface?.readiness || {};
    const blockers = readiness.blockers || [];
    const rows = datasets.map(ds => {
        const installed = ds.installed;
        const state = installed?.state;
        let badge;
        if (state === "pending_remove") {
            badge = `<span class="plugin-dataset-status msg-error">${t("plugin.dataset_pending_remove")}</span>`;
        } else if (installed) {
            const blocker = blockers.find(b =>
                (b.kind === "dataset_corrupt" || b.kind === "dataset_outdated") && b.field === ds.id);
            if (blocker?.kind === "dataset_corrupt") {
                badge = `<span class="plugin-dataset-status msg-error">${t("plugin.dataset_corrupt")}</span>`;
            } else if (blocker?.kind === "dataset_outdated") {
                badge = `<span class="plugin-dataset-status msg-error">${t("plugin.dataset_outdated")}</span>`;
            } else {
                badge = `<span class="plugin-dataset-status plugin-dataset-ready">v${escapeHtml(installed.version || "")}</span>`;
            }
        } else {
            badge = `<span class="plugin-dataset-status">${ds.optional
                ? t("plugin.dataset_missing_optional") : t("plugin.dataset_missing")}</span>`;
        }
        const catEntry = catalog?.datasets?.find(
            (d) => d.plugin_id === plugin.id && d.dataset_id === ds.id);
        const buttons = [];
        if (catEntry) {
            buttons.push(`<button class="btn-small plugin-dataset-install" data-dataset="${escapeAttr(ds.id)}" data-catalog="1" data-progress-id="${escapeAttr(`${plugin.id}/${ds.id}`)}">${iconHTML("download")} ${installed ? t("plugin.dataset_update") : t("plugin.dataset_install")}</button>`);
        } else if (devMode) {
            buttons.push(`<button class="btn-small plugin-dataset-install" data-dataset="${escapeAttr(ds.id)}">${iconHTML("download")} ${installed ? t("plugin.dataset_update") : t("plugin.dataset_install")}</button>`);
        } else {
            buttons.push(`<span class="plugin-dataset-status plugin-catalog-stale" title="${escapeAttr(t("plugin.catalog_unavailable"))}">${t("plugin.dataset_no_source")}</span>`);
        }
        if (devMode && catEntry) {
            buttons.push(`<button class="btn-small plugin-dataset-install-local" data-dataset="${escapeAttr(ds.id)}" title="${escapeAttr(t("plugin.dataset_local_hint"))}">${iconHTML("folder-open")} ${t("plugin.dataset_local")}</button>`);
        }
        if (installed && state !== "pending_remove") {
            buttons.push(`<button class="btn-small plugin-dataset-uninstall" data-dataset="${escapeAttr(ds.id)}">${t("plugin.dataset_uninstall")}</button>`);
        }
        return `<div class="plugin-dataset-row" data-dataset="${escapeAttr(ds.id)}">
          <span class="plugin-dataset-id">${escapeHtml(ds.id)}${ds.optional ? `<em class="plugin-dataset-optional">${t("plugin.dataset_optional_tag")}</em>` : ""}</span>
          ${badge}
          <span class="plugin-surface-buttons">${buttons.join("")}</span>
        </div>`;
    }).join("");
    return `<div class="plugin-datasets-section">
      <span class="trigger-label">${t("plugin.dataset_section")}</span>
      ${rows}
    </div>`;
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
      <input type="text" class="trigger-add-inline-input hidden" placeholder="${t("plugin.trigger_placeholder")}" />
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
    <input type="text" class="trigger-add-inline-input hidden" placeholder="${t("plugin.trigger_placeholder")}" />`,
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
            (field.options || []).forEach((o) => {
                optionsMap[o.value] = o.label;
            });
            control = renderSortableList(field.key, items, optionsMap);
            break;
        }
        case "number":
            control = `<div class="number-input-wrapper"><input type="number" class="plugin-field" data-key="${field.key}" value="${escapeAttr(val ?? "")}" ${field.min != null ? `min="${field.min}"` : ""} ${field.max != null ? `max="${field.max}"` : ""} /><div class="number-spinner"><button type="button" class="spinner-up" aria-label="${t("spinner.increase")}">▲</button><button type="button" class="spinner-down" aria-label="${t("spinner.decrease")}">▼</button></div></div>`;
            break;
        case "string":
        default: {
            // 0.25.16: secret 字段渲染 password + 不回显——真实值在系统凭据管理器，
            // 输入框恒空（空=未修改）；掩码 hint 由 bindSecretFields 异步填充 placeholder。
            // 非 secret 字段保持原样回显 settings 值。
            if (field.secret) {
                control = `<input type="password" class="plugin-field plugin-secret-field" data-key="${field.key}" value="" autocomplete="new-password" placeholder="${escapeAttr(t("plugin.secret_placeholder"))}" />` +
                    `<button type="button" class="btn-small plugin-secret-clear" data-key="${escapeAttr(field.key)}">${escapeHtml(t("plugin.secret_clear"))}</button>`;
            } else {
                control = `<input type="text" class="plugin-field" data-key="${field.key}" value="${escapeAttr(val ?? "")}" />`;
            }
            break;
        }
    }
    const descIcon = field.description
        ? `<span class="field-hint-icon" title="${escapeAttr(field.description)}">ⓘ</span>`
        : "";

    // no_space 字段（密钥类）：输入含空格时显示红字提示 + 一键移除按钮。
    // 不自动改写用户输入——密钥是否含空格由用户确认，避免静默修改引入意外。
    const spaceWarn = field.type === "string" && field.no_space
        ? `<div class="plugin-field-warn hidden" data-warn-key="${escapeAttr(field.key)}">
             <span class="plugin-field-warn-text msg-error"></span>
             <button type="button" class="btn-small plugin-field-warn-fix">${t("plugin.field_space_fix")}</button>
           </div>`
        : "";

    if (useSettingRow) {
        return `
      <div class="setting-row" data-key="${escapeAttr(field.key)}">
        <label class="setting-label">${escapeHtml(field.title)}${descIcon}</label>
        ${control}
        ${spaceWarn}
      </div>`;
    }

    return `
    <div class="plugin-field-row" data-key="${escapeAttr(field.key)}">
      <div class="field-head">
        <span class="field-title">${escapeHtml(field.title)}${descIcon}</span>
        <div class="plugin-field-control"><div class="plugin-field-input">${control}</div>${spaceWarn}</div>
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
        return `<div class="sortable-item" data-value="${escapeAttr(val)}">
      <span class="sortable-handle" aria-hidden="true">⠿</span>
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
 * 初始化可拖动列表事件。
 *
 * WebView2 的原生 HTML5 drag 在无边框窗口中不稳定，因此统一走鼠标拖拽：
 * 整行均可抓取，越过相邻项中线时立即重排 DOM，直接展示最终落点预览。
 */
function initSortableLists() {
    if (initSortableLists._bound) return;
    initSortableLists._bound = true;

    let _mouseDrag = null;
    let _mouseClone = null;
    let _mouseStartX = 0;
    let _mouseStartY = 0;
    let _pointerOffsetY = 0;
    let _dragActive = false;

    function startDragPreview(item) {
        const rect = item.getBoundingClientRect();
        _dragActive = true;
        item.classList.add("dragging");
        item.closest(".sortable-list")?.classList.add("is-dragging");

        _mouseClone = item.cloneNode(true);
        _mouseClone.classList.remove("dragging");
        _mouseClone.classList.add("sortable-drag-preview");
        _mouseClone.style.width = rect.width + "px";
        _mouseClone.style.left = rect.left + "px";
        _mouseClone.style.top = rect.top + "px";
        document.body.appendChild(_mouseClone);
    }

    function previewDropPosition(list, clientY) {
        const siblings = [...list.querySelectorAll(".sortable-item:not(.dragging)")];
        const before = siblings.find((item) => {
            const rect = item.getBoundingClientRect();
            return clientY < rect.top + rect.height / 2;
        });
        if (before) list.insertBefore(_mouseDrag, before);
        else list.appendChild(_mouseDrag);
    }

    document.addEventListener("mousedown", (e) => {
        if (e.button !== 0 || e.target.closest("button, input, select, textarea, a")) return;
        const item = e.target.closest(".sortable-item");
        if (!item) return;
        e.preventDefault();

        _mouseDrag = item;
        _mouseStartX = e.clientX;
        _mouseStartY = e.clientY;
        const rect = item.getBoundingClientRect();
        _pointerOffsetY = e.clientY - rect.top;
    });

    document.addEventListener("mousemove", (e) => {
        if (!_mouseDrag) return;
        if (!_dragActive) {
            const moved = Math.hypot(e.clientX - _mouseStartX, e.clientY - _mouseStartY);
            if (moved < 4) return;
            startDragPreview(_mouseDrag);
        }
        e.preventDefault();
        _mouseClone.style.top = (e.clientY - _pointerOffsetY) + "px";

        const list = _mouseDrag.closest(".sortable-list");
        if (!list) return;
        previewDropPosition(list, e.clientY);
    });

    document.addEventListener("mouseup", (e) => {
        if (!_mouseDrag) return;
        const list = _mouseDrag.closest(".sortable-list");

        if (list && _dragActive) {
            updateSortableValue(list);
        }

        _mouseDrag.classList.remove("dragging");
        list?.classList.remove("is-dragging");
        if (_mouseClone) {
            _mouseClone.remove();
            _mouseClone = null;
        }
        _mouseDrag = null;
        _dragActive = false;
    });
}

/**
 * 更新可拖动列表的值到隐藏 input
 * 手动派发 change 事件，让上层自动保存生效（否则 hidden input 赋值不冒泡）
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
    input.dispatchEvent(new Event("change", {bubbles: true}));
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
    return {type: "string", key, title, ...opts};
}

/** 快捷构建布尔字段 schema */
function booleanField(key, title, opts = {}) {
    return {type: "boolean", key, title, ...opts};
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
        return `<details class="plugin-group" data-group="${escapeAttr(group.title)}">
      <summary class="plugin-group-title">
        <span>${escapeHtml(group.title)}</span>
        <span class="plugin-group-status">${t("plugin.group_empty")}</span>
      </summary>
      <div class="plugin-group-body">${descHtml}${fieldsHtml}</div>
    </details>`;
    }).join("");

    // 反馈消息 span 必须存在（flash 依赖）：有保存按钮时嵌在保存行，
    // 无按钮时单独渲染——空 span 不占位（min-height:0），出现提示才撑开
    const saveRow = opts.saveLabel
        ? `<div class="plugin-save-row">
         <button class="btn-small plugin-save">${escapeHtml(opts.saveLabel)}</button>
         <span class="plugin-save-msg"></span>
       </div>`
        : `<span class="plugin-save-msg"></span>`;

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
         ${opts.extraHtml || ""}
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
function renderExtensionCard({icon, title, desc, headerRight = "", attrs = "", classes = "", body}) {
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
 * 启用脚本插件时的托管运行时安装引导（0.25.20）。
 *
 * python / node 脚本插件跑 Blink 托管解释器——启用时若未安装，弹确认
 * （大小与用途），确认后深链到 引擎 Tab 的脚本运行时区块手动安装；
 * 取消则插件保持启用，查询时由四层兜底返回引导文案。
 * @param {"python"|"node"|undefined} runtimeType - manifest runtime 类型
 */
async function guideScriptRuntimeInstall(runtimeType) {
    if (runtimeType !== "python" && runtimeType !== "node") return;
    try {
        const list = await invoke("script_interpreters_status");
        const st = (list || []).find((x) => x.kind === runtimeType);
        if (st?.installed) return;
        const label = t(`engine.${runtimeType === "python" ? "python" : "node"}.title`);
        const go = await confirmDialog(
            t("plugin.script_runtime.guide", {runtime: label}),
            {title: t("engine.script_runtime.title"), kind: "info"},
        );
        if (!go) return;
        await navigateSettings({tabId: "managed-engines", target: "#script-runtime-card", block: "start"});
        // 深链定位已等待目标就绪（waitForSettingsTarget）——按钮直接可用；
        // 注意力闪烁 3 次脉冲让用户知道要点哪个（animationend 自动移除）
        const btn = document.getElementById(`script-${runtimeType}-action`);
        if (btn && !btn.disabled) {
            btn.classList.add("attention-flash");
            btn.addEventListener("animationend", () => btn.classList.remove("attention-flash"), {once: true});
        }
    } catch (e) {
        console.error("guideScriptRuntimeInstall failed:", e);
    }
}

/**
 * 绑定单个插件卡片事件（enabled 开关 + 保存 settings + 触发词增删禁用）
 */
export function bindPluginCardEvents(plugin) {
    const card = document.querySelector(`.extension-card[data-plugin-id="${plugin.id}"]`);
    if (!card) return;
    const id = plugin.id;
    const schema = plugin.settings_schema || [];

    const weatherEditor = id === "builtin.weather" ? mountWeatherSettings(card, plugin) : null;
    if (weatherEditor) {
        weatherEditors.set(id,weatherEditor);
        card.addEventListener(WEATHER_EDITOR_CLOSED,()=> {if(pendingPluginReload) loadPlugins();});
    }
    let writeQueue = Promise.resolve();
    let configConflict = false;
    const enqueue = (operation) => {
        pluginWrites++;
        const task = writeQueue.then(async () => {
            try {
                if (configConflict) return false;
                await operation();
                return true;
            } catch (error) {
                console.error("plugin config write failed:", error);
                flash(card, t("common.save_failed_msg", {err: error?.message || String(error)}), true, 6000);
                // Keep drafts visible on a revision conflict. Never overwrite them with a reload.
                if (error?.code === "config_conflict" || String(error?.message || error).includes("config_conflict")) {
                    configConflict = true;
                    card.dataset.configConflict = "true";
                    flash(card, t("weather.save_conflict"), true, 0);
                    const reload = document.createElement("button");
                    reload.type = "button";
                    reload.className = "btn-small plugin-reload";
                    reload.textContent = t("plugin.reload_settings");
                    reload.addEventListener("click", async () => {
                        await loadPlugins(true);
                        const section = document.querySelector(`.extension-card[data-plugin-id="${id}"] details.plugin-config-section`);
                        if (section) section.open = true;
                    });
                    card.querySelector(".plugin-save-msg")?.appendChild(reload);
                }
                return false;
            } finally {
                pluginWrites--;
                if (!pluginWrites && pendingPluginReload) void loadPlugins();
            }
        });
        writeQueue = task;
        return task;
    };
    const mutateTriggers = (command, args) => enqueue(async () => {
        const revision = await invoke(command, {pluginId: id, ...args, expectedRevision: plugin.revision ?? 0});
        if (!Number.isSafeInteger(revision) || revision <= (plugin.revision ?? 0)) throw new Error("invalid plugin revision");
        plugin.revision = revision;
    });
    const save = (patch, enabledOverride) => enqueue(async () => {
        const enabled = enabledOverride ?? plugin.enabled ?? true;
        await saveConfig("plugin_config", {pluginId: id, enabled, settings: patch, expectedRevision: plugin.revision ?? 0});
        plugin.revision = (plugin.revision ?? 0) + 1;
        plugin.settings = {...plugin.settings, ...patch};
        plugin.enabled = enabled;
        for (const field of schema) {
            if (!field.secret || !Object.hasOwn(patch, field.key)) continue;
            plugin.settings[field.key] = "";
            const input = card.querySelector(`.plugin-field[data-key="${field.key}"]`);
            if (input?.value === patch[field.key]) input.value = "";
            await input?._refreshHint?.();
            input?._refreshSpaceHint?.();
        }
        refreshCredentialGroups(card, schema, plugin.settings);
        flash(card, t("common.saved"), false);
    });
    card.querySelectorAll(".weather-save, .plugin-save").forEach(button => button.addEventListener("click", async () => {
        button.disabled = true;
        const patch = weatherEditor?.collect() || collectSettings(card, schema, {});
        const ok = await save(patch);
        if (ok) weatherEditor?.commit(patch);
        if (button.isConnected) button.disabled = false;
    }));
    card.querySelector(".plugin-enabled")?.addEventListener("change", async (event) => {
        const enabled = event.target.checked;
        const ok = await save({}, enabled);
        if (!ok) event.target.checked = plugin.enabled !== false;
        if (ok) {
            card.classList.toggle("is-disabled", !enabled);
            weatherEditor?.setEnabled(enabled);
            if (enabled) guideScriptRuntimeInstall(plugin.runtime_type);
        }
    });
    // 0.28.2：打开插件独立窗口（宿主入口；重复唤起后端复用同一窗口）
    card.querySelectorAll(".plugin-open-window").forEach(button => button.addEventListener("click", async () => {
        try {
            await invoke("open_plugin_page", {pluginId: id, pageId: button.dataset.page || null});
        } catch (error) {
            flash(card, error?.message || String(error), true, 4000);
        }
    }));
    // 0.28.5：数据包安装/更新。data-catalog=1 → 官方目录下载（主通道）；
    // 无标记 / -local 后缀 → dev 本地 zip（仅开发模式出现，生产由后端门禁兜底）
    card.querySelectorAll(".plugin-dataset-install[data-catalog='1']").forEach(button => button.addEventListener("click", () => {
        installFromCatalog(button, "dataset",
            {pluginId: id, datasetId: button.dataset.dataset}, `${id}/${button.dataset.dataset}`);
    }));
    card.querySelectorAll(".plugin-dataset-install:not([data-catalog='1']), .plugin-dataset-install-local").forEach(button => button.addEventListener("click", async () => {
        button.disabled = true;
        try {
            const path = await openDialog({
                title: t("plugin.dataset_pick_title"),
                filters: [{name: "Blink dataset", extensions: ["zip"]}],
                multiple: false,
                directory: false,
            });
            if (!path) return;
            const record = await invoke("install_plugin_dataset", {path});
            flash(card, t("plugin.dataset_installed_ok", {id: record.dataset_id, version: record.version}), false, 4000);
            await loadPlugins(true);
        } catch (error) {
            flash(card, commandErrorText(error, t("plugin.dataset_install_failed")), true, 6000);
        } finally {
            button.disabled = false;
        }
    }));
    // 0.28.4：数据包卸载（二次确认；文件被占用时如实提示待清理）
    card.querySelectorAll(".plugin-dataset-uninstall").forEach(button => button.addEventListener("click", async () => {
        const datasetId = button.dataset.dataset;
        if (!await confirmDialog(t("plugin.dataset_uninstall_confirm", {id: datasetId}), {kind: "warning"})) return;
        button.disabled = true;
        try {
            const result = await invoke("uninstall_plugin_dataset", {pluginId: id, datasetId});
            flash(card, result?.pending_files
                ? t("plugin.dataset_uninstall_pending", {count: result.pending_files})
                : t("plugin.dataset_uninstalled"), result?.pending_files ? true : false, 4000);
            await loadPlugins(true);
        } catch (error) {
            flash(card, commandErrorText(error, t("plugin.dataset_uninstall_failed")), true, 6000);
        } finally {
            button.disabled = false;
        }
    }));
    // 未就绪徽标 → 展开配置区（修复入口：缺密钥等在配置区补齐）
    card.querySelectorAll(".plugin-surface-blocked").forEach(badge => badge.addEventListener("click", () => {
        const section = card.querySelector("details.plugin-config-section");
        if (section) section.open = true;
        section?.querySelector("input, select")?.focus?.();
    }));
    card.querySelectorAll(".plugin-field").forEach(input => {
        input.addEventListener("change", async () => {
            applyFieldVisibility(card, schema);
            const field = schema.find(field => field.key === input.dataset.key);
            if (!field) return;
            const patch = collectSettings(card, [field], {});
            if (field.key === "qweather_api_host" && patch[field.key]) {
                const value = patch[field.key].trim();
                patch[field.key] = (value.includes("://") ? value : `https://${value}`).replace(/\/$/, "");
                input.value = patch[field.key];
                input.setSelectionRange?.(input.value.length, input.value.length);
                input.scrollLeft = input.scrollWidth;
            }
            if (Object.keys(patch).length) await save(patch);
        });
    });
    bindCredentialFields(card, plugin, schema, enqueue, flash);
    applyFieldVisibility(card, schema);

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
                if (!await mutateTriggers("toggle_default_trigger", {keyword, disabled: !isDisabled})) {
                    btn.innerHTML = originalContent;
                    return;
                }
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
                if (await mutateTriggers("delete_custom_trigger", {keyword})) tag.remove();
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
                addInputInline.classList.remove('hidden');
                addInputInline.focus();
                btn.classList.add('hidden');
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
                if (await mutateTriggers("delete_custom_trigger", {keyword: kw})) newTag.remove();
            } catch (err) {
                console.error("delete_custom_trigger failed:", err);
            }
        });
    }

    async function commitAdd(inputEl) {
        const kw = (inputEl.value || "").trim();
        if (!kw) {
            inputEl.classList.add('hidden');
            if (addBtnInline) addBtnInline.classList.remove('hidden');
            if (addBtnText) addBtnText.classList.remove('hidden');
            return;
        }
        try {
            if (!await mutateTriggers("add_custom_trigger", {keyword: kw})) return;
            appendCustomTag(kw);
        } catch (err) {
            console.error("add_custom_trigger failed:", err);
        }
        inputEl.value = "";
        inputEl.classList.add('hidden');
        if (addBtnInline) addBtnInline.classList.remove('hidden');
        if (addBtnText) addBtnText.classList.remove('hidden');
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
function collectSettings(card, schema, baseline = {}) {
    const settings = {...(baseline || {})};
    for (const f of schema) {
        if (f.secret) delete settings[f.key];
        if (f.type === "sortable_list") {
            settings[f.key] = collectSortableValue(card, f.key);
            continue;
        }
        const el = card.querySelector(`.plugin-field[data-key="${f.key}"]`);
        if (!el) continue;
        // 0.25.16: secret 字段空 = 未修改（不发送，后端不动 CM）；
        // 非空 = 新密钥（发送，后端转存 CM 并脱敏落库）
        if (f.secret && el.value === "") continue;
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

/**
 * 应用 schema 的条件显隐（visible_when）：引用字段值等于条件值时才显示该行。
 * 例:翻译插件"降级顺序"仅在"允许降级"开关开启时显示。
 */
function applyFieldVisibility(card, schema) {
    for (const f of schema) {
        if (!f.visible_when) continue;
        const row = card.querySelector(`[data-key="${CSS.escape(f.key)}"].plugin-field-row, [data-key="${CSS.escape(f.key)}"].setting-row`);
        if (!row) continue;
        const ref = card.querySelector(`.plugin-field[data-key="${CSS.escape(f.visible_when.key)}"]`);
        if (!ref) continue;
        const current = ref.type === "checkbox" ? ref.checked : ref.value;
        row.classList.toggle("hidden", String(current) !== String(f.visible_when.equals));
    }
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
        input.dispatchEvent(new Event("change", {bubbles: true}));
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
    return String(s ?? "").replace(/[&<>"']/g, (c) => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;"
    }[c]));
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
        await invoke("open_url", {url});
    } catch (e) {
        console.error("openExternalUrl failed:", e);
        window.open(url, "_blank");
    }
}

/** 卡片内显示一行反馈（默认 2s 后清除；警告/错误类可传更长时长） */
function flash(card, msg, isError, duration = 2000) {
    const el = card.querySelector(".plugin-save-msg");
    if (!el) return;
    el.textContent = msg;
    el.className = `plugin-save-msg ${isError ? "msg-error" : "msg-success"}`;
    clearTimeout(el._t);
    if (duration === 0) return;
    el._t = setTimeout(() => {
        el.textContent = "";
        el.className = "plugin-save-msg";
    }, duration);
}
