/**
 * 能力与操作 Tab 模块（0.21.6 重写：三出口矩阵表格）
 *
 * 矩阵布局：行 = 功能，列 = 本地 / AI / MCP 三出口开关。
 * - 本地列：binding 聚合启停（写回 disabled_builtin_actions /
 *   disabled_context_bindings / disabled_chord_actions 三分片，§3.7 binding store 真源）
 * - AI 列：ai.capability_access allowlist（toggle_ai_capability）
 * - MCP 列：exposed_capabilities（set_mcp_server_config 热更新）
 * - 代码级禁止 / Interaction-only / 无本地入口的出口显示 "—"，不可点击
 * - Chord 键位不在目录内修改，跳转 Chord 设置页（§3.7）
 *
 * 纯逻辑（状态推导 / 过滤 / op 构造）在 capabilities-core.js，可单测。
 * 数据刷新：订阅 blink://config-changed（本页自身写入也会触发广播，统一走重取）。
 */
import { invoke, listen } from "../../shared/tauri.js";
import { iconHTML } from "../../shared/icon.js";
import { EVENTS } from "../../shared/event-names.js";
import { t, onLangChange } from "../../i18n/index.js";
import {
  GROUP_ORDER,
  groupKeyOf,
  sourceClassOf,
  riskOf,
  localToggleState,
  exitToggleState,
  buildBindingOps,
  nextMcpExposed,
  matchesFilters,
  groupLocalStatus,
} from "./capabilities-core.js";

/** 目录缓存：feature_id → FeatureCatalogItem（applyVisibility 数据驱动过滤用） */
let catalogById = new Map();
/** 目录顺序保持后端分组排序 */
let catalogOrdered = [];
/** 加载竞态 epoch：旧响应不得覆盖新渲染（spec-frontend §5.1） */
let loadEpoch = 0;
/** 事件只注册一次 */
let eventsRegistered = false;

/**
 * 初始化能力与操作 Tab
 */
export function initCapabilitiesTab() {
  loadCatalog();
  initControls();

  if (!eventsRegistered) {
    eventsRegistered = true;
    // 后端 config-changed 广播统一触发重取（本页 toggle / 其他页写入 / 语言切换）
    listen(EVENTS.CONFIG_CHANGED, () => {
      loadCatalog();
    });
    onLangChange(() => {
      render();
    });
  }
}

// ── 数据加载 ──────────────────────────────────────────────────────────────────

async function loadCatalog() {
  const container = document.getElementById("capabilities-container");
  if (!container) return;

  const epoch = ++loadEpoch;
  let catalog;
  try {
    catalog = await invoke("list_feature_catalog");
  } catch (e) {
    console.error("loadCatalog failed:", e);
    if (epoch !== loadEpoch) return;
    container.innerHTML = `<p class="hint msg-error" style="padding: 12px 0;">${escapeHtml(String(e))}</p>`;
    return;
  }
  if (epoch !== loadEpoch) return; // 过期响应丢弃

  catalogOrdered = Array.isArray(catalog) ? catalog : [];
  catalogById = new Map(catalogOrdered.map((f) => [f.feature_id, f]));
  render();
  applyVisibility();
}

// ── 渲染 ──────────────────────────────────────────────────────────────────────

function render() {
  const container = document.getElementById("capabilities-container");
  if (!container) return;

  if (catalogOrdered.length === 0) {
    container.innerHTML = `<p class="hint" style="padding: 12px 0;">${t("capabilities.empty")}</p>`;
    return;
  }

  const groups = new Map(GROUP_ORDER.map((g) => [g, []]));
  for (const feature of catalogOrdered) {
    groups.get(groupKeyOf(feature)).push(feature);
  }

  container.innerHTML = [...groups.entries()]
    .filter(([, features]) => features.length > 0)
    .map(([key, features]) => renderGroup(key, features))
    .join("");

  // 部分启用的本地开关设置 indeterminate 三态
  container
    .querySelectorAll(".cap-toggle[data-partial]")
    .forEach((el) => {
      el.indeterminate = true;
    });

  bindContainerEvents(container);
}

/** 单个分组：标题 + 三态标记 + 组级批量（本地列）+ 矩阵表 */
function renderGroup(groupKey, features) {
  const state = groupLocalStatus(features);
  const stateLabel = {
    all_enabled: t("capabilities.group.enable_all"),
    all_disabled: t("capabilities.group.disable_all"),
    partial: t("capabilities.group.partial"),
    none: "—",
  }[state];

  return `
    <section class="capability-group" data-group="${groupKey}">
      <header class="cap-group-header">
        <div class="cap-group-title">
          <h3>${t(`capabilities.group.${groupKey}`)}</h3>
          <span class="cap-group-state">${stateLabel}</span>
        </div>
        <div class="cap-group-ops">
          <button class="btn-link btn-small cap-group-op" data-group="${groupKey}" data-op="enable">${t("capabilities.group.enable_all")}</button>
          <button class="btn-link btn-small cap-group-op" data-group="${groupKey}" data-op="disable">${t("capabilities.group.disable_all")}</button>
        </div>
      </header>
      <div class="cap-table">
        ${renderHeadRow()}
        ${features.map(renderFeatureRow).join("")}
      </div>
    </section>
  `;
}

/** 矩阵列：功能 | 本地 | AI | MCP | 详情（列宽定义在 settings.css .cap-item） */

function renderHeadRow() {
  return `
    <div class="cap-item cap-head">
      <div class="cap-cell cap-cell-label">${t("capabilities.column.feature")}</div>
      <div class="cap-cell cap-cell-label">${t("capabilities.status.local")}</div>
      <div class="cap-cell cap-cell-label">${t("capabilities.status.ai")}</div>
      <div class="cap-cell cap-cell-label">${t("capabilities.status.mcp")}</div>
      <div class="cap-cell"></div>
    </div>
  `;
}

/** 单个功能行 + 可折叠高级详情行 */
function renderFeatureRow(feature) {
  const fid = feature.feature_id;
  const bindings = feature.bindings || [];

  return `
    <div class="cap-item" data-fid="${escapeAttr(fid)}">
      <div class="cap-cell cap-main">
        <div class="cap-title-line">
          <span class="cap-title" title="${escapeAttr(feature.title || fid)}">${escapeHtml(feature.title || fid)}</span>
          <span class="badge badge-${sourceClassOf(feature)}">${t(`capabilities.source.${sourceClassOf(feature)}`)}</span>
          <span class="badge badge-${riskOf(feature)}">${t(`capabilities.risk.${riskOf(feature)}`)}</span>
        </div>
        ${feature.description ? `<div class="cap-desc">${escapeHtml(feature.description)}</div>` : ""}
        ${bindings.length > 0 ? `<div class="cap-bindings">${bindings.map(renderBindingChip).join("")}</div>` : ""}
      </div>
      <div class="cap-cell cap-exit">${renderToggleCell(feature, "local")}</div>
      <div class="cap-cell cap-exit">${renderToggleCell(feature, "ai")}</div>
      <div class="cap-cell cap-exit">${renderToggleCell(feature, "mcp")}</div>
      <div class="cap-cell cap-more">
        <button class="cap-details-btn" data-fid="${escapeAttr(fid)}" title="${t("capabilities.action.show_details")}">
          ${iconHTML("chevron-down")}
        </button>
      </div>
      <div class="cap-detail hidden">
        ${renderAdvancedDetails(feature)}
      </div>
    </div>
  `;
}

/** 本地 binding 摘要 chip；chord 键位提供跳转编辑（§3.7 目录内不改键） */
function renderBindingChip(binding) {
  const label = escapeHtml(binding.trigger_label || binding.binding_id);
  if (binding.kind === "chord_key") {
    const chordId = binding.binding_id.replace(/^chord\./, "");
    return `<button class="cap-binding cap-binding-chord" data-chord-id="${escapeAttr(chordId)}" title="${t("capabilities.action.edit_key")}">${label}</button>`;
  }
  const stateClass = binding.enabled ? "" : " cap-binding-off";
  return `<span class="cap-binding${stateClass}">${label}</span>`;
}

/** 出口单元格：toggle 或 "—"（代码禁止 / 无投影 / 无本地入口） */
function renderToggleCell(feature, exit) {
  const state = exit === "local" ? localToggleState(feature) : exitToggleState(feature, exit);

  if (state.kind === "dash") {
    const hint = {
      code_forbidden: t("capabilities.status.policy_forbidden_hint"),
      no_projection: t("capabilities.status.not_applicable_hint"),
      no_binding: t("capabilities.status.no_binding_hint"),
      unavailable: feature.unavailable_reason || t("capabilities.status.unavailable"),
    }[state.reason] || "";
    return `<span class="cap-dash" title="${escapeAttr(hint)}">—</span>`;
  }

  // 部分启用的本地开关标记三态（indeterminate 无法用 HTML 属性表达，渲染后设置）
  const partial =
    exit === "local" &&
    !state.checked &&
    (feature.bindings || []).some((b) => b.enabled);

  return `
    <label class="switch switch-sm" title="${escapeAttr(toggleHint(exit))}">
      <input type="checkbox" class="cap-toggle" data-fid="${escapeAttr(feature.feature_id)}" data-exit="${exit}" ${state.checked ? "checked" : ""} ${partial ? 'data-partial="1"' : ""}>
      <span class="slider"></span>
    </label>
  `;
}

/** 开关悬停说明 */
function toggleHint(exit) {
  if (exit === "ai") return t("capabilities.hint.ai_toggle");
  if (exit === "mcp") return t("capabilities.hint.mcp_toggle");
  return t("capabilities.hint.local_toggle");
}

/** 高级详情：id / capability / runtime / 确认策略 / 不可用原因 / binding ids */
function renderAdvancedDetails(feature) {
  const p = feature.capability_projection;
  const rows = [
    [t("capabilities.details.feature_id"), feature.feature_id],
    [t("capabilities.details.capability_id"), feature.capability_id || "—"],
    p ? [t("capabilities.details.runtime"), p.runtime_requirement] : null,
    p && p.requires_confirmation
      ? [t("capabilities.details.confirmation"), t("capabilities.details.confirmation_required")]
      : null,
    feature.unavailable_reason
      ? [t("capabilities.details.unavailable_reason"), feature.unavailable_reason]
      : null,
    (feature.bindings || []).length > 0
      ? [
          t("capabilities.details.bindings"),
          feature.bindings.map((b) => `${b.kind}: ${b.binding_id}`).join("  ·  "),
        ]
      : null,
  ].filter(Boolean);

  return `
    <dl class="cap-detail-list">
      ${rows
        .map(
          ([k, v]) =>
            `<div class="cap-detail-row"><dt>${escapeHtml(k)}</dt><dd>${escapeHtml(String(v))}</dd></div>`,
        )
        .join("")}
    </dl>
  `;
}

// ── 事件 ──────────────────────────────────────────────────────────────────────

/** 容器级事件委托（重渲染后无需重复绑定；以标志位防重复注册） */
function bindContainerEvents(container) {
  if (container.dataset.eventsBound) return;
  container.dataset.eventsBound = "1";

  // 三出口开关
  container.addEventListener("change", (e) => {
    const toggle = e.target.closest(".cap-toggle");
    if (!toggle) return;
    handleToggle(toggle.dataset.fid, toggle.dataset.exit, toggle.checked);
  });

  container.addEventListener("click", (e) => {
    // 高级详情展开/收起
    const detailsBtn = e.target.closest(".cap-details-btn");
    if (detailsBtn) {
      const item = detailsBtn.closest(".cap-item");
      const detail = item?.querySelector(".cap-detail");
      if (detail) {
        detail.classList.toggle("hidden");
        detailsBtn.classList.toggle("cap-details-open");
      }
      return;
    }
    // chord 键位跳转 Chord 设置页
    const chordBtn = e.target.closest(".cap-binding-chord");
    if (chordBtn) {
      jumpToChordSettings(chordBtn.dataset.chordId);
      return;
    }
    // 组级批量（本地列）
    const groupOp = e.target.closest(".cap-group-op");
    if (groupOp) {
      handleGroupOp(groupOp.dataset.group, groupOp.dataset.op);
    }
  });
}

/** 单个出口开关写回（失败回滚 UI → 重取目录） */
async function handleToggle(fid, exit, enabled) {
  const feature = catalogById.get(fid);
  if (!feature) return;

  try {
    if (exit === "local") {
      const ops = buildBindingOps(feature, enabled);
      if (ops.length > 0) {
        await invoke("apply_binding_ops", { ops });
      }
    } else if (exit === "ai" && feature.capability_id) {
      await invoke("toggle_ai_capability", {
        capabilityId: feature.capability_id,
        enabled,
      });
    } else if (exit === "mcp" && feature.capability_id) {
      const config = await invoke("get_mcp_server_config");
      config.exposed_capabilities = nextMcpExposed(
        config.exposed_capabilities,
        feature.capability_id,
        enabled,
      );
      await invoke("set_mcp_server_config", { config });
    }
    // 成功路径依赖后端 config-changed 广播统一刷新
  } catch (e) {
    console.error("handleToggle failed:", fid, exit, e);
    alert(`${t("capabilities.error.toggle_failed")}: ${e}`);
    loadCatalog(); // 回滚开关到真实状态
  }
}

/** 组级批量：对组内全部可启停功能的本地 binding 应用 enable/disable */
async function handleGroupOp(groupKey, op) {
  const enable = op === "enable";
  const ops = catalogOrdered
    .filter((f) => groupKeyOf(f) === groupKey && localToggleState(f).kind === "toggle")
    .flatMap((f) => buildBindingOps(f, enable));

  if (ops.length === 0) return;
  try {
    await invoke("apply_binding_ops", { ops });
  } catch (e) {
    console.error("handleGroupOp failed:", groupKey, op, e);
    alert(`${t("capabilities.error.group_operation_failed")}: ${e}`);
    loadCatalog();
  }
}

/** 跳转 Chord 设置页并定位到对应行（chord.js 渲染 data-chord-id） */
function jumpToChordSettings(chordId) {
  document.querySelector('[data-tab="chord"]')?.click();
  setTimeout(() => {
    const row = document.querySelector(`[data-chord-id="${CSS.escape(chordId || "")}"]`);
    row?.scrollIntoView({ behavior: "smooth", block: "center" });
  }, 100);
}

// ── 搜索与过滤（组合式，数据驱动） ────────────────────────────────────────────

function initControls() {
  const search = document.getElementById("capabilities-search");
  if (search && !search.dataset.bound) {
    search.dataset.bound = "1";
    search.addEventListener("input", debounce(() => applyVisibility(), 250));
  }
  for (const id of ["filter-source", "filter-risk", "filter-availability", "filter-exit"]) {
    const el = document.getElementById(id);
    if (el && !el.dataset.bound) {
      el.dataset.bound = "1";
      el.addEventListener("change", applyVisibility);
    }
  }
}

/** 读取控件当前值，组合过滤所有行；空组整节隐藏 */
function applyVisibility() {
  const container = document.getElementById("capabilities-container");
  if (!container) return;

  const filters = {
    query: document.getElementById("capabilities-search")?.value || "",
    source: document.getElementById("filter-source")?.value || "all",
    risk: document.getElementById("filter-risk")?.value || "all",
    availability: document.getElementById("filter-availability")?.value || "all",
    exit: document.getElementById("filter-exit")?.value || "all",
  };

  for (const item of container.querySelectorAll(".cap-item:not(.cap-head)")) {
    const feature = catalogById.get(item.dataset.fid);
    const visible = feature ? matchesFilters(feature, filters) : false;
    item.classList.toggle("hidden", !visible);
  }

  for (const group of container.querySelectorAll(".capability-group")) {
    const anyVisible = group.querySelectorAll(".cap-item:not(.cap-head):not(.hidden)").length > 0;
    group.classList.toggle("hidden", !anyVisible);
  }
}

// ── 工具 ──────────────────────────────────────────────────────────────────────

function debounce(func, wait) {
  let timeout;
  return (...args) => {
    clearTimeout(timeout);
    timeout = setTimeout(() => func(...args), wait);
  };
}

function escapeHtml(text) {
  const div = document.createElement("div");
  div.textContent = text == null ? "" : String(text);
  return div.innerHTML;
}

function escapeAttr(text) {
  return escapeHtml(text).replace(/"/g, "&quot;");
}
