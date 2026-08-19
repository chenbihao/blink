/**
 * Composer bar 悬浮预览 popup（0.13.8）。
 *
 * 悬浮在 composer bar 右侧圆圈进度条上时弹出一个三段式详情面板：
 * - 上：上下文容量（进度条 + token 数 + 提示词统计）
 * - 中：内置工具（数量 + 可折叠名称列表，默认折叠）
 * - 下：MCP 服务（在线/离线状态 + tool 数量，默认折叠 tool 列表）
 *
 * 数据来源：后端 `get_composer_bar_snapshot` 命令（一次 IPC 聚合）。
 * 悬浮时 lazy 加载，缓存 3 秒避免频繁 IPC。
 */

import {getComposerBarSnapshot} from "./ipc.js";
import {escapeAttr, escapeText} from "./utils.js";
import * as state from "./state.js";

/** popup DOM 元素 */
let popupEl = null;

/** hover 触发区域（composer bar 右侧 context-indicator 圆圈） */
let triggerZone = null;

/** 缓存的快照数据 + 过期时间 + 拉取时的 conversationId */
let cachedSnapshot = null;
let cacheExpiry = 0;
let cachedConversationId = null;
const CACHE_TTL_MS = 3000;

/** hover 延迟（ms），避免快速划过触发 */
const HOVER_DELAY = 150;
/** leave 延迟（ms），给鼠标移动留缓冲时间 */
const LEAVE_DELAY = 300;
let hoverTimer = null;
let leaveTimer = null;

/**
 * 初始化 composer bar popup。
 * 在 main.js init() 中调用。
 */
export function initComposerBarPopup() {
    // 触发区域改为只绑定圆圈进度条
    triggerZone = document.getElementById("chat-context-indicator") ||
        document.querySelector(".chat-composer-bar-right");
    if (!triggerZone) return;

    // 创建 popup 容器（默认 hidden）
    popupEl = document.createElement("div");
    popupEl.className = "composer-bar-popup";
    popupEl.hidden = true;
    popupEl.innerHTML = `<div class="composer-bar-popup-loading">加载中...</div>`;
    document.body.appendChild(popupEl);

    // hover 进入触发区域
    triggerZone.addEventListener("mouseenter", () => {
        clearTimeout(leaveTimer);
        hoverTimer = setTimeout(() => showPopup(), HOVER_DELAY);
    });

    // hover 离开触发区域
    triggerZone.addEventListener("mouseleave", (e) => {
        clearTimeout(hoverTimer);
        // 如果鼠标移到了 popup 上，不关闭
        if (popupEl.contains(e.relatedTarget)) return;
        // 延迟关闭，给鼠标移到 popup 上的时间
        leaveTimer = setTimeout(() => hidePopup(), LEAVE_DELAY);
    });

    // popup 鼠标进入 → 取消关闭
    popupEl.addEventListener("mouseenter", () => {
        clearTimeout(leaveTimer);
    });

    // popup 鼠标离开 → 延迟关闭
    popupEl.addEventListener("mouseleave", (e) => {
        // 如果鼠标移回了触发区域，不关闭
        if (triggerZone.contains(e.relatedTarget)) return;
        leaveTimer = setTimeout(() => hidePopup(), LEAVE_DELAY);
    });

    // 点击外部关闭
    document.addEventListener("click", (e) => {
        if (!popupEl.hidden && !popupEl.contains(e.target) && !triggerZone.contains(e.target)) {
            hidePopup();
        }
    });

    // 折叠/展开 事件委托
    popupEl.addEventListener("click", (e) => {
        const header = e.target.closest(".cbp-collapsible-header");
        if (!header) return;
        const section = header.parentElement;
        if (section.hasAttribute("data-collapsed")) {
            section.removeAttribute("data-collapsed");
        } else {
            section.setAttribute("data-collapsed", "");
        }
    });
}

/**
 * 显示 popup 并加载快照数据。
 */
async function showPopup() {
    if (!popupEl) return;

    // 定位 popup（在触发区域上方）
    positionPopup();

    // 显示 loading
    popupEl.innerHTML = `<div class="composer-bar-popup-loading">加载中...</div>`;
    popupEl.hidden = false;

    // 加载数据
    const snapshot = await loadSnapshot();
    if (!snapshot) {
        popupEl.innerHTML = `<div class="composer-bar-popup-error">加载失败</div>`;
        return;
    }

    renderPopup(snapshot);
}

/**
 * 隐藏 popup。
 */
function hidePopup() {
    if (popupEl) popupEl.hidden = true;
}

/**
 * 定位 popup——在触发区域上方，右对齐。
 */
function positionPopup() {
    const rect = triggerZone.getBoundingClientRect();
    const popupWidth = 360;
    const popupMaxHeight = 400;

    // 水平：右对齐触发区域右边
    const right = window.innerWidth - rect.right;
    popupEl.style.right = `${Math.max(8, right)}px`;

    // 垂直：在触发区域上方
    const spaceAbove = rect.top;
    if (spaceAbove > popupMaxHeight + 16) {
        // 上方空间足够
        popupEl.style.bottom = `${window.innerHeight - rect.top + 4}px`;
        popupEl.style.top = "auto";
    } else {
        // 上方不够，放在下方
        popupEl.style.top = `${rect.bottom + 4}px`;
        popupEl.style.bottom = "auto";
    }
}

/**
 * 加载快照（带 3 秒缓存）。
 */
async function loadSnapshot() {
    const now = Date.now();
    const currentConvId = state.conversationId;
    // P0-2: 当前 conversationId 与缓存不一致时视为失效，重新拉取
    if (cachedSnapshot && now < cacheExpiry && cachedConversationId === currentConvId) {
        return cachedSnapshot;
    }

    try {
        const snapshot = await getComposerBarSnapshot(currentConvId);
        cachedSnapshot = snapshot;
        cacheExpiry = now + CACHE_TTL_MS;
        cachedConversationId = currentConvId;
        return snapshot;
    } catch (e) {
        console.error("[composer-bar-popup] 加载快照失败:", e);
        return null;
    }
}

/**
 * 清除缓存（MCP 拓扑变化后调用）。
 */
export function invalidateComposerBarCache() {
    cachedSnapshot = null;
    cacheExpiry = 0;
    cachedConversationId = null;
}

/**
 * 0.13.8: 如果 popup 当前可见，立即重新拉取快照并重新渲染。
 *
 * 供 ensure_mcp_connected 完成后调用——之前只清缓存（invalidateComposerBarCache），
 * 但如果 popup 正在显示，用户需要关闭再重新 hover 才能看到更新。
 * 现在直接原地刷新，用户无需手动关闭/重开。
 */
export async function refreshPopupIfVisible() {
    if (!popupEl || popupEl.hidden) return;
    // 强制跳过缓存
    cachedSnapshot = null;
    cacheExpiry = 0;
    cachedConversationId = null;
    const snapshot = await loadSnapshot();
    if (snapshot) {
        renderPopup(snapshot);
    }
}

/**
 * 渲染 popup 内容（三段式：上下文 / 内置工具 / MCP 服务）。
 */
function renderPopup(snapshot) {
    const ctxHtml = renderContextSection(snapshot);
    const builtinHtml = renderBuiltinSection(snapshot);
    const mcpHtml = renderMcpSection(snapshot);

    popupEl.innerHTML = `
    <div class="cbp-section cbp-context">${ctxHtml}</div>
    <div class="cbp-divider"></div>
    <div class="cbp-section cbp-builtin" data-collapsed>${builtinHtml}</div>
    <div class="cbp-divider"></div>
    <div class="cbp-section cbp-mcp">${mcpHtml}</div>
  `;

    // 重新定位（内容高度可能变了）
    positionPopup();
}

/**
 * 格式化 token 数（万级别用万单位）。
 */
export function fmtTokens(n) {
    if (n >= 10000) {
        return `${(n / 10000).toFixed(1)}万`;
    }
    return n.toLocaleString();
}

/**
 * 上：上下文容量（含提示词统计）。
 */
export function renderContextSection(s) {
    const percent = Math.min(s.usage_percent, 100);
    const limit = s.context_limit || 0;
    const tokens = s.estimated_tokens || 0;
    const preambleTokens = s.preamble_tokens || 0;
    const pendingTokens = s.pending_message_tokens || 0;
    // 0.21.17: 优先使用后端提供的 history_tokens，否则回退计算
    const historyTokens = s.history_tokens != null
        ? s.history_tokens
        : Math.max(0, tokens - preambleTokens - pendingTokens);
    const toolsTokens = s.tools_tokens || 0;
    const protocolOverhead = s.protocol_overhead_tokens || 0;
    const multimodalTokens = s.multimodal_tokens || 0;
    const reservedOutput = s.reserved_output_tokens || 0;
    const safetyMargin = s.safety_margin_tokens || 0;
    const effectiveInputLimit = s.effective_input_limit || 0;
    const remainingTokens = s.remaining_tokens != null ? s.remaining_tokens : Math.max(0, effectiveInputLimit - tokens);
    const contextLimitSource = s.context_limit_source || "";
    const confidence = s.confidence || "";

    if (limit === 0) {
        return `
      <div class="cbp-section-header">
        <span class="cbp-section-title">上下文容量</span>
        <span class="cbp-section-count">未开始</span>
      </div>
      <div class="cbp-context-empty">发送消息后将显示 token 用量</div>
    `;
    }

    const color = percent < 60 ? "var(--text-faint)" : percent < 80 ? "var(--warning, #ffc107)" : "var(--danger, #f44336)";

    // 0.21.17: context limit 来源标签
    const sourceLabel = contextLimitSource === "fallback"
        ? `<span class="cbp-context-source cbp-context-source-fallback" title="模型未配置 context window，使用 32K 保守回退值">估算</span>`
        : "";
    // 0.21.17: 置信度标签
    const confidenceLabel = confidence === "low"
        ? `<span class="cbp-confidence cbp-confidence-low" title="包含多模态内容，估算精度较低">低精度</span>`
        : confidence === "medium"
        ? `<span class="cbp-confidence" title="包含工具定义，估算精度中等">中精度</span>`
        : "";

    return `
    <div class="cbp-section-header">
      <span class="cbp-section-title">上下文容量</span>
      <span class="cbp-section-count">${percent}%${sourceLabel}${confidenceLabel}</span>
    </div>
    <div class="cbp-context-bar">
      <div class="cbp-context-bar-fill" style="width: ${percent}%; background: ${color}"></div>
    </div>
    <div class="cbp-context-detail">
      <span>${fmtTokens(tokens)} / ${fmtTokens(limit)} tokens</span>
    </div>
    <div class="cbp-context-breakdown">
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">历史消息</span>
        <span class="cbp-breakdown-value">${fmtTokens(historyTokens)}</span>
      </div>
      ${preambleTokens > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">系统提示词</span>
        <span class="cbp-breakdown-value">${fmtTokens(preambleTokens)}</span>
      </div>` : ""}
      ${pendingTokens > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">当前消息</span>
        <span class="cbp-breakdown-value">${fmtTokens(pendingTokens)}</span>
      </div>` : ""}
      ${toolsTokens > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">工具定义</span>
        <span class="cbp-breakdown-value">${fmtTokens(toolsTokens)}</span>
      </div>` : ""}
      ${protocolOverhead > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">协议开销</span>
        <span class="cbp-breakdown-value">${fmtTokens(protocolOverhead)}</span>
      </div>` : ""}
      ${multimodalTokens > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">多模态</span>
        <span class="cbp-breakdown-value">${fmtTokens(multimodalTokens)}</span>
      </div>` : ""}
      <div class="cbp-breakdown-divider"></div>
      ${reservedOutput > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">输出预留</span>
        <span class="cbp-breakdown-value">${fmtTokens(reservedOutput)}</span>
      </div>` : ""}
      ${safetyMargin > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">安全余量</span>
        <span class="cbp-breakdown-value">${fmtTokens(safetyMargin)}</span>
      </div>` : ""}
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">安全剩余</span>
        <span class="cbp-breakdown-value cbp-breakdown-remaining">${fmtTokens(remainingTokens)}</span>
      </div>
      ${s.last_compressed ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">已压缩</span>
        <span class="cbp-breakdown-value">${s.last_compressed_count} 条</span>
      </div>` : ""}
      ${s.last_recall_count > 0 ? `
      <div class="cbp-breakdown-row">
        <span class="cbp-breakdown-label">已召回</span>
        <span class="cbp-breakdown-value">${s.last_recall_count} 条</span>
      </div>` : ""}
    </div>
  `;
}

/**
 * 中：内置工具（默认折叠，点击展开）。
 */
function renderBuiltinSection(s) {
    const tools = s.builtin_tools || [];
    if (tools.length === 0) {
        return `
      <div class="cbp-section-header">
        <span class="cbp-section-title">内置工具</span>
        <span class="cbp-section-count">0</span>
      </div>
      <div class="cbp-empty">无</div>
    `;
    }

    const toolChips = tools
        .map((t) => `<span class="cbp-tool-chip" title="${escapeAttr(t.description)}">${escapeText(t.name)}</span>`)
        .join("");

    return `
    <div class="cbp-collapsible-header">
      <span class="cbp-section-title">内置工具</span>
      <span class="cbp-section-count">${s.builtin_count}</span>
      <span class="cbp-collapse-arrow">▸</span>
    </div>
    <div class="cbp-collapsible-body">
      <div class="cbp-tool-chips">${toolChips}</div>
    </div>
  `;
}

/**
 * 下：MCP 服务（tool 列表默认折叠在每个 server 内）。
 */
function renderMcpSection(s) {
    const servers = s.mcp_servers || [];
    if (servers.length === 0) {
        return `
      <div class="cbp-section-header">
        <span class="cbp-section-title">MCP 服务</span>
        <span class="cbp-section-count">0</span>
      </div>
      <div class="cbp-empty">未配置 MCP server</div>
    `;
    }

    const serverHtml = servers
        .map((srv) => {
            const dot = srv.online
                ? `<span class="cbp-mcp-dot cbp-mcp-online"></span>`
                : `<span class="cbp-mcp-dot cbp-mcp-offline"></span>`;

            return `<div class="cbp-mcp-row" data-online="${srv.online}">
        ${dot}
        <span class="cbp-mcp-name">${escapeText(srv.name)}</span>
        <span class="cbp-mcp-transport">${srv.transport}</span>
        <span class="cbp-mcp-count">${srv.tool_count}</span>
      </div>`;
        })
        .join("");

    return `
    <div class="cbp-section-header">
      <span class="cbp-section-title">MCP 服务</span>
      <span class="cbp-section-count">${s.mcp_count}</span>
    </div>
    <div class="cbp-mcp-list">${serverHtml}</div>
  `;
}
