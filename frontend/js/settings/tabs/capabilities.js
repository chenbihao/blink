/**
 * 能力与操作 Tab 模块（0.21.10 重设计：组块矩阵 + 风险列 + 姿态摘要）
 *
 * 布局：全页唯一 sticky 表头 + 每组一个独立圆角块（块间留白），
 * 列 = 功能 | 风险 | 本地 | AI | MCP | 详情；
 * 组头行放大（字体/控件）且与本表列对齐，每列出一只组级批量开关——
 * 默认隐藏、悬浮组行时显示（部分启用时半选），按列生效：
 * - 本地列：binding 聚合启停（写回 disabled_builtin_actions /
 *   disabled_context_bindings / disabled_chord_actions 三分片，§3.7 binding store 真源）
 * - AI 列：ai.capability_access allowlist（toggle_ai_capability / 组级 toggle_ai_capabilities）
 * - MCP 列：exposed_capabilities（set_mcp_server_config 热更新）
 * - 代码级禁止 / Interaction-only / 无本地入口的出口显示 "—"，不可点击
 * - 风险列：仅非默认态出图标（敏感=eye / 危险=triangle-alert），危险能力对 AI/MCP
 *   开放时开关变警示色；Chord 键位不在目录内修改，跳转 Chord 设置页（§3.7）
 * - Context 门禁动作（open_url 等）本地入口标签为 context 触发条件
 *   （裸 trigger key，按 context.trigger.* 翻译），不展示唤不起的关键词
 * - 顶部姿态摘要行：AI/MCP 授权计数 + 危险警示 + "恢复推荐"（偏离推荐态才出现，
 *   推荐态 = §4.1b 修订：本地全启用 / AI 非危险全开 / MCP 仅暴露安全能力）
 *
 * 纯逻辑（状态推导 / 过滤 / op 构造 / 推荐态 diff）在 capabilities-core.js，可单测。
 * 数据刷新：订阅 blink://config-changed（本页自身写入也会触发广播，统一走重取）。
 */
import {confirmDialog, invoke, listen} from "../../shared/tauri.js";
import {iconHTML} from "../../shared/icon.js";
import {EVENTS} from "../../shared/event-names.js";
import {onLangChange, t} from "../../i18n/index.js";
import {
    applyMcpChanges,
    buildBindingOps,
    exitToggleState,
    GROUP_ORDER,
    groupAiOps,
    groupExitState,
    groupKeyOf,
    groupLocalOps,
    groupMcpChanges,
    localToggleState,
    matchesFilters,
    nextMcpExposed,
    postureSummary,
    recommendedDiff,
    riskOf,
    sourceClassOf,
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
        container.innerHTML = `<p class="hint msg-error cap-inline-hint">${escapeHtml(String(e))}</p>`;
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
        container.innerHTML = `<p class="hint cap-inline-hint">${t("capabilities.empty")}</p>`;
        return;
    }

    const groups = new Map(GROUP_ORDER.map((g) => [g, []]));
    for (const feature of catalogOrdered) {
        groups.get(groupKeyOf(feature)).push(feature);
    }

    const blocks = [];
    for (const [key, features] of groups) {
        if (features.length === 0) continue;
        blocks.push(renderGroupBlock(key, features));
    }

    container.innerHTML = `
    ${renderPosture()}
    <div class="cap-table">
      ${renderHeadRow()}
      ${blocks.join("")}
    </div>
    <p class="hint cap-no-match hidden">${t("capabilities.no_match")}</p>
  `;

    // 部分启用的开关设置 indeterminate 三态（行级本地 + 组级悬浮开关）
    container
        .querySelectorAll("input[data-partial]")
        .forEach((el) => {
            el.indeterminate = true;
        });

    bindContainerEvents(container);
}

/**
 * 姿态摘要行：AI/MCP 授权计数 + 危险能力警示 + 恢复推荐（偏离推荐态才出现）。
 */
function renderPosture() {
    const s = postureSummary(catalogOrdered);
    const warn =
        s.dangerousAiOn > 0
            ? `<span class="cap-posture-warn">${iconHTML("triangle-alert")}${escapeHtml(t("capabilities.posture.dangerous_ai", {count: s.dangerousAiOn}))}</span>`
            : "";
    const tail = recommendedDiff(catalogOrdered).isClean
        ? `<span class="cap-posture-clean">${escapeHtml(t("capabilities.posture.clean"))}</span>`
        : `<button class="btn-link cap-reset-btn">${iconHTML("rotate-ccw")}<span>${escapeHtml(t("capabilities.posture.reset"))}</span></button>`;
    return `
    <div class="cap-posture">
      <span>${escapeHtml(t("capabilities.posture.ai", {on: s.aiOn, total: s.aiTotal}))}</span>
      <span class="cap-posture-dot">·</span>
      <span>${escapeHtml(t("capabilities.posture.mcp", {on: s.mcpOn, total: s.mcpTotal}))}</span>
      ${warn}
      <span class="cap-posture-spacer"></span>
      ${tail}
    </div>
  `;
}

/** 表头：功能 | 风险 | 本地 | AI | MCP |（详情列无标签）——列宽定义在 settings.css .cap-row */
function renderHeadRow() {
    return `
    <div class="cap-row cap-head">
      <div class="cap-cell">${t("capabilities.column.feature")}</div>
      <div class="cap-cell cap-risk-head">${t("capabilities.column.risk")}</div>
      <div class="cap-cell cap-exit-head">${t("capabilities.status.local")}</div>
      <div class="cap-cell cap-exit-head">${t("capabilities.status.ai")}</div>
      <div class="cap-cell cap-exit-head">${t("capabilities.status.mcp")}</div>
      <div class="cap-cell"></div>
    </div>
  `;
}

/**
 * 组块：组头行 + 组内功能行，独立圆角块（块间留白由 .cap-table gap 提供）。
 */
function renderGroupBlock(groupKey, features) {
    return `
    <div class="cap-group-block" data-group-block="${groupKey}">
      ${renderGroupRow(groupKey, features)}
      ${features.map((f) => renderFeatureRow(f)).join("")}
    </div>
  `;
}

/**
 * 组头行：组名 + 计数 + 每列一只组级批量开关（与本表列对齐，按列生效）。
 * 组行整体放大（字体/控件），开关默认隐藏、悬浮组行时才显示——
 * 静态层级只靠"大组行 vs 小行"区分，悬浮出现的开关天然是组级操作。
 * 组内没有该出口可操作行时该列留空。
 */
function renderGroupRow(groupKey, features) {
    const cells = ["local", "ai", "mcp"]
        .map((exit) => renderGroupToggleCell(groupKey, features, exit))
        .join("");
    return `
    <div class="cap-row cap-group-row" data-group-row="${groupKey}">
      <div class="cap-cell cap-group-name">
        <span>${t(`capabilities.group.${groupKey}`)}</span>
        <span class="cap-group-count">${t("capabilities.group_count", {count: features.length})}</span>
      </div>
      <div class="cap-cell"></div>
      ${cells}
      <div class="cap-cell"></div>
    </div>
  `;
}

/** 组级列开关（三出口各一只；none 时留空）。半选由渲染后 indeterminate 表达 */
function renderGroupToggleCell(groupKey, features, exit) {
    const state = groupExitState(features, exit);
    if (state.kind === "none") return `<div class="cap-cell"></div>`;
    const partial = state.partial ? ' data-partial="1"' : "";
    return `
    <div class="cap-cell cap-exit">
      <label class="switch switch-sm cap-group-toggle" title="${escapeAttr(t(`capabilities.group_toggle.${exit}`))}">
        <input type="checkbox" data-group="${groupKey}" data-exit="${exit}" ${state.checked ? "checked" : ""}${partial}>
        <span class="slider"></span>
      </label>
    </div>
  `;
}

/** 单个功能行 + 可折叠高级详情行 */
function renderFeatureRow(feature) {
    const fid = feature.feature_id;
    const groupKey = groupKeyOf(feature);
    const bindings = feature.bindings || [];

    // 非默认来源才出徽章（内置是大多数行的默认态，逐行重复只会稀释层级）；
    // 风险已移至独立图标列，不再占用标题行
    const sourceClass = sourceClassOf(feature);
    const badges =
        sourceClass !== "builtin"
            ? `<span class="badge badge-${sourceClass}">${t(`capabilities.source.${sourceClass}`)}</span>`
            : "";

    return `
    <div class="cap-row" data-fid="${escapeAttr(fid)}" data-group="${groupKey}">
      <div class="cap-cell cap-main">
        <div class="cap-title-line">
          <span class="cap-title" title="${escapeAttr(feature.title || fid)}">${escapeHtml(feature.title || fid)}</span>
          ${badges}
          ${bindings.length > 0 ? `<span class="cap-bindings">${bindings.map(renderBindingChip).join("")}</span>` : ""}
        </div>
        ${feature.description ? `<div class="cap-desc">${escapeHtml(feature.description)}</div>` : ""}
      </div>
      ${renderRiskCell(feature)}
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

/**
 * 风险列：仅非默认态出图标（敏感=eye / 危险=triangle-alert），safe 留空。
 * 颜色走 --warning / --red 主题 token，悬停 title 出完整风险名。
 */
function renderRiskCell(feature) {
    const risk = riskOf(feature);
    if (risk === "safe") return `<div class="cap-cell cap-risk"></div>`;
    const label = t(`capabilities.risk.${risk}`);
    const icon = risk === "dangerous" ? "triangle-alert" : "eye";
    return `<div class="cap-cell cap-risk cap-risk-${risk}" title="${escapeAttr(label)}">${iconHTML(icon, {ariaLabel: label})}</div>`;
}

/**
 * 本地 binding 摘要 chip。
 * - chord 键位：链接样式，点击跳转 Chord 设置编辑（§3.7 目录内不改键）
 * - Context 门禁动作：label 是裸 trigger key，按 context.trigger.* 翻译为
 *   "剪贴板是 URL" 等人类可读条件；不展示唤不起的关键词
 * - 其余（关键词等）：原样展示，超长由 CSS 截断 + title 全文
 */
function renderBindingChip(binding) {
    const label = binding.trigger_label || binding.binding_id;
    if (binding.kind === "chord_key") {
        const chordId = binding.binding_id.replace(/^chord\./, "");
        return `<button class="cap-binding cap-binding-chord" data-chord-id="${escapeAttr(chordId)}" title="${t("capabilities.action.edit_key")}">${escapeHtml(label)}</button>`;
    }
    if (/^[a-z][a-z0-9_]*$/.test(label)) {
        const key = `context.trigger.${label}`;
        const translated = t(key);
        const text = translated !== key ? translated : label;
        return `<span class="cap-binding cap-binding-ctx" title="${escapeAttr(t("capabilities.binding.context_hint"))}">${escapeHtml(text)}</span>`;
    }
    const stateClass = binding.enabled ? "" : " cap-binding-off";
    return `<span class="cap-binding${stateClass}" title="${escapeAttr(label)}">${escapeHtml(label)}</span>`;
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

    // 危险能力对 AI/MCP 开放：开关着警示色（决策后果可视化，本地入口用户主动触发不着色）
    const dangerOn =
        (exit === "ai" || exit === "mcp") &&
        state.checked &&
        feature.capability_projection?.danger === "dangerous";

    return `
    <label class="switch switch-sm" title="${escapeAttr(toggleHint(exit))}">
      <input type="checkbox" class="cap-toggle${dangerOn ? " cap-toggle-dangerous" : ""}" data-fid="${escapeAttr(feature.feature_id)}" data-exit="${exit}" ${state.checked ? "checked" : ""} ${partial ? 'data-partial="1"' : ""}>
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

    // 三出口开关（行级 switch + 组级悬浮开关共用 change 委托；
    // 组级开关 change 由其内部 input 冒泡，dataset 就在 e.target 上）
    container.addEventListener("change", (e) => {
        if (e.target.closest("label.cap-group-toggle")) {
            handleGroupToggle(e.target.dataset.group, e.target.dataset.exit, e.target.checked);
            return;
        }
        const toggle = e.target.closest(".cap-toggle");
        if (toggle) {
            handleToggle(toggle.dataset.fid, toggle.dataset.exit, toggle.checked);
        }
    });

    container.addEventListener("click", (e) => {
        // 恢复推荐（偏离推荐态时渲染）
        if (e.target.closest(".cap-reset-btn")) {
            handleResetRecommended();
            return;
        }
        // 高级详情展开/收起
        const detailsBtn = e.target.closest(".cap-details-btn");
        if (detailsBtn) {
            const item = detailsBtn.closest(".cap-row");
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
                await invoke("apply_binding_ops", {ops});
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
            await invoke("set_mcp_server_config", {config});
        }
        // 成功路径依赖后端 config-changed 广播统一刷新
    } catch (e) {
        console.error("handleToggle failed:", fid, exit, e);
        alert(`${t("capabilities.error.toggle_failed")}: ${e}`);
        loadCatalog(); // 回滚开关到真实状态
    }
}

/**
 * 组级列开关写回（按列生效，写各自真源；失败回滚 UI → 重取目录）。
 * - local → apply_binding_ops（binding 三分片）
 * - ai → toggle_ai_capabilities（allowlist 批量）
 * - mcp → get/set_mcp_server_config（exposed_capabilities 批量增删）
 */
async function handleGroupToggle(groupKey, exit, enabled) {
    const features = catalogOrdered.filter((f) => groupKeyOf(f) === groupKey);

    try {
        if (exit === "local") {
            const ops = groupLocalOps(features, enabled);
            if (ops.length > 0) {
                await invoke("apply_binding_ops", {ops});
            }
        } else if (exit === "ai") {
            const ops = groupAiOps(features, enabled);
            if (ops.length > 0) {
                await invoke("toggle_ai_capabilities", {ops});
            }
        } else if (exit === "mcp") {
            const changes = groupMcpChanges(features, enabled);
            if (changes.add.length > 0 || changes.remove.length > 0) {
                const config = await invoke("get_mcp_server_config");
                config.exposed_capabilities = applyMcpChanges(
                    config.exposed_capabilities,
                    changes,
                );
                await invoke("set_mcp_server_config", {config});
            }
        }
        // 成功路径依赖后端 config-changed 广播统一刷新
    } catch (e) {
        console.error("handleGroupToggle failed:", groupKey, exit, e);
        alert(`${t("capabilities.error.group_operation_failed")}: ${e}`);
        loadCatalog(); // 回滚开关到真实状态
    }
}

/**
 * 恢复推荐：把全部三出口重置为 §4.1b 推荐态（本地全启用 / AI 非危险全开 /
 * MCP 仅暴露安全能力），覆盖用户在此页的全部微调，执行前确认。写回复用既有批量命令。
 */
async function handleResetRecommended() {
    const diff = recommendedDiff(catalogOrdered);
    if (diff.isClean) return;

    const ok = await confirmDialog(t("capabilities.reset.confirm"), {
        title: t("capabilities.posture.reset"),
    });
    if (!ok) return;

    try {
        if (diff.bindingOps.length > 0) {
            await invoke("apply_binding_ops", {ops: diff.bindingOps});
        }
        if (diff.aiOps.length > 0) {
            await invoke("toggle_ai_capabilities", {ops: diff.aiOps});
        }
        if (diff.mcpAdd.length > 0 || diff.mcpRemove.length > 0) {
            const config = await invoke("get_mcp_server_config");
            config.exposed_capabilities = applyMcpChanges(
                config.exposed_capabilities,
                {add: diff.mcpAdd, remove: diff.mcpRemove},
            );
            await invoke("set_mcp_server_config", {config});
        }
        // 成功路径依赖后端 config-changed 广播统一刷新（姿态摘要随之更新为"与推荐一致"）
    } catch (e) {
        console.error("handleResetRecommended failed:", e);
        alert(`${t("capabilities.error.group_operation_failed")}: ${e}`);
        loadCatalog();
    }
}

/** 跳转 Chord 设置页并定位到对应行（chord.js 渲染 data-chord-id） */
function jumpToChordSettings(chordId) {
    document.querySelector('[data-tab="chord"]')?.click();
    setTimeout(() => {
        const row = document.querySelector(`[data-chord-id="${CSS.escape(chordId || "")}"]`);
        row?.scrollIntoView({behavior: "smooth", block: "center"});
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

/** 读取控件当前值，组合过滤所有行；空组隐藏组分隔行；全空显示无匹配提示 */
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

    for (const item of container.querySelectorAll(".cap-row[data-fid]")) {
        const feature = catalogById.get(item.dataset.fid);
        const visible = feature ? matchesFilters(feature, filters) : false;
        item.classList.toggle("hidden", !visible);
    }

    // 组块：组内无可见行时整块隐藏（含组头行；组 key 为 GROUP_ORDER 白名单值，可安全内插）
    for (const block of container.querySelectorAll(".cap-group-block")) {
        const key = block.dataset.groupBlock;
        const anyVisible =
            block.querySelectorAll(
                `.cap-row[data-fid][data-group="${key}"]:not(.hidden)`,
            ).length > 0;
        block.classList.toggle("hidden", !anyVisible);
    }

    const noMatch = container.querySelector(".cap-no-match");
    if (noMatch) {
        const total = container.querySelectorAll(".cap-row[data-fid]:not(.hidden)").length;
        noMatch.classList.toggle("hidden", total > 0);
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
