/**
 * MCP Tab 模块（0.13.0 → 0.13.7 重构）
 *
 * 参照 ZCode 设计：
 * - 列表：绿灯/红灯状态 + 工具数 + 错误信息 + 传输类型
 * - 每行右侧只有：开关 / 编辑 / 删除
 * - 编辑弹窗：表单/JSON 双 tab
 * - 进入页面时自动探测所有 server 状态
 *
 * 后端 commands:
 * - list_mcp_servers — 列出所有 server（含状态）
 * - upsert_mcp_server — 添加/更新 server 配置
 * - delete_mcp_server — 删除 server（同时停止）
 * - set_mcp_server_enabled — 切换 enabled
 * - test_mcp_connection — 测试连接（与预热/prompt 共用 single-flight）
 * - get_mcp_server_tools — 获取 tool 列表
 * - set_mcp_server_disabled_tools — 更新 tool 可见性
 * - get_mcp_tool_pool_size — 获取 tool 池规模
 */
import {confirmDialog, invoke, messageDialog} from "../../shared/tauri.js";
import {t} from "../../i18n/index.js";
import {iconHTML} from "../../shared/icon.js";

/**
 * 初始化 MCP Tab
 */
export function initMcPTab() {
    loadServerList();
    initFormHandlers();
    initImportHandlers();
}

// ── Server 列表加载与渲染 ──────────────────────────────────────────────────────

/**
 * 加载并渲染 server 列表。
 *
 * 0.13.7: 进入页面时自动逐个探测所有 server 状态（test_connection），
 * 显示绿灯/红灯 + 工具数 + 错误信息。成功连接保留并进入正常 tool pool。
 */
async function loadServerList() {
    const container = document.getElementById("mcp-server-list");
    if (!container) return;

    try {
        const servers = await invoke("list_mcp_servers");
        if (!servers || servers.length === 0) {
            container.innerHTML =
                '<p class="mcp-empty-hint">' + t("ai.mcp.empty") + '</p>';
            return;
        }

        // 渲染列表（先显示「探测中」状态）
        container.innerHTML = `
      <p class="mcp-list-summary">已配置 MCP 服务器 ${servers.length} 项</p>
    ` + servers.map(s => renderServerRow(s, true)).join("");

        // 绑定事件
        servers.forEach(s => bindServerRowEvents(s.name));

        // 异步探测每个 server（不阻塞 UI）
        // 已禁用的 server 不探测，直接显示「已禁用」状态
        for (const s of servers) {
            if (s.enabled) {
                probeServer(s.name);
            } else {
                updateServerRowStatus(s.name, "disabled", 0, null, null);
            }
        }
    } catch (e) {
        container.innerHTML = '<p class="mcp-error">' + t("ai.mcp.load_failed") + ': ' + escapeHtml(e) + '</p>';
        console.error("loadServerList failed:", e);
    }
}

/**
 * 局部新增/更新单个 server 行（不重新加载整个列表，避免全量重新探测）。
 *
 * - config 已存在（编辑）：原地替换该行 HTML + 重新探测该行；
 * - config 不存在（新增）：追加到列表末尾 + 探测该行。
 * 参考 delete 操作（card.remove()）的局部更新范式。
 *
 * @param {object} config McpServerConfig（与 upsert_mcp_server 入参一致）
 */
async function upsertServerRow(config) {
    const container = document.getElementById("mcp-server-list");
    if (!container || !config || !config.name) return;

    // 若列表当前是空提示，清掉
    const emptyHint = container.querySelector(".mcp-empty-hint");
    if (emptyHint && !container.querySelector(".mcp-server-card")) {
        container.innerHTML = "";
    }

    const existing = container.querySelector(
        `.mcp-server-card[data-name="${cssEscape(config.name)}"]`
    );

    if (existing) {
        // 编辑：原地替换该行（保留位置），重新探测
        const tmp = document.createElement("div");
        tmp.innerHTML = renderServerRow(config, true).trim();
        const newRow = tmp.firstElementChild;
        if (newRow) {
            existing.replaceWith(newRow);
            bindServerRowEvents(config.name);
            if (config.enabled) {
                probeServer(config.name);
            } else {
                updateServerRowStatus(config.name, "disabled", 0, null, null);
            }
        }
    } else {
        // 新增：追加到列表末尾（summary 之后）
        let summary = container.querySelector(".mcp-list-summary");
        if (!summary) {
            container.insertAdjacentHTML(
                "afterbegin",
                '<p class="mcp-list-summary">已配置 MCP 服务器 0 项</p>'
            );
            summary = container.querySelector(".mcp-list-summary");
        }
        summary.insertAdjacentHTML("afterend", renderServerRow(config, true).trim());
        bindServerRowEvents(config.name);
        if (config.enabled) {
            probeServer(config.name);
        } else {
            updateServerRowStatus(config.name, "disabled", 0, null, null);
        }
    }
    refreshListSummary();
}

/**
 * 异步探测单个 server 状态（test_connection）。
 * 探测后更新对应行的绿灯/红灯 + 工具数 + 错误信息。
 */
async function probeServer(name) {
    const row = document.querySelector(`.mcp-server-card[data-name="${cssEscape(name)}"]`);
    if (!row) return;

    // 先显示探测中状态（黄灯 + 探测中...）
    updateServerRowStatus(name, "probing", 0, null, null);

    try {
        const tools = await invoke("test_mcp_connection", {name});
        // 绿灯 + 工具数
        updateServerRowStatus(name, "online", tools.length, null, tools);
    } catch (e) {
        // 红灯 + 错误信息
        updateServerRowStatus(name, "offline", 0, String(e), null);
    }
}

/**
 * 更新 server 行的状态显示。
 *
 * status 取值：
 * - "probing" — 探测中（黄灯 + 探测中...）
 * - "online"  — 已连接（绿灯 + 工具数）
 * - "offline" — 离线/连接失败（红灯 + 短错误，点击短错误展开/折叠详细报错）
 * - "disabled"— 已禁用（灰灯 + 清除错误）
 */
function updateServerRowStatus(name, status, toolCount, errorMsg, tools) {
    const row = document.querySelector(`.mcp-server-card[data-name="${cssEscape(name)}"]`);
    if (!row) return;

    const dot = row.querySelector(".mcp-status-dot");
    const toolInfo = row.querySelector(".mcp-tool-count");
    const errEl = row.querySelector(".mcp-error-msg");
    const toolsDiv = row.querySelector(".mcp-server-tools");

    if (status === "probing") {
        if (dot) {
            dot.className = "mcp-status-dot mcp-dot-probing";
        }
        if (toolInfo) {
            toolInfo.textContent = "探测中...";
        }
        if (errEl) {
            errEl.textContent = "";
            errEl.classList.add('hidden');
            errEl.onclick = null;
        }
        if (toolsDiv) {
            toolsDiv.classList.add('hidden');
        }
        row._cachedTools = null;
        row._errorMsg = null;
    } else if (status === "online") {
        if (dot) {
            dot.className = "mcp-status-dot mcp-dot-online";
        }
        if (toolInfo) {
            toolInfo.textContent = `${toolCount} 个工具`;
        }
        if (errEl) {
            errEl.textContent = "";
            errEl.classList.add('hidden');
            errEl.onclick = null;
        }
        if (toolsDiv) {
            toolsDiv.classList.add('hidden');
        }
        row._cachedTools = tools;
        row._errorMsg = null;
    } else if (status === "disabled") {
        if (dot) {
            dot.className = "mcp-status-dot mcp-dot-disabled";
        }
        if (toolInfo) {
            toolInfo.textContent = "已禁用";
        }
        if (errEl) {
            errEl.textContent = "";
            errEl.classList.add('hidden');
            errEl.onclick = null;
        }
        if (toolsDiv) {
            toolsDiv.classList.add('hidden');
        }
        row._cachedTools = null;
        row._errorMsg = null;
    } else {
        // offline
        if (dot) {
            dot.className = "mcp-status-dot mcp-dot-offline";
        }
        if (toolInfo) {
            toolInfo.textContent = "0 个工具";
        }
        if (errEl && errorMsg) {
            // 短错误信息在 header 中默认展示，点击展开下方 tools 区域显示详细报错
            errEl.textContent = `加载失败: server ${name} 未连接或不存在`;
            errEl.classList.remove('hidden');
            errEl.style.cursor = "pointer";
            // 缓存详细报错，供 toggleTools 渲染
            row._errorMsg = errorMsg;
            row._cachedTools = null;
            // 点击短错误信息展开下方 tools 区域
            errEl.onclick = (e) => {
                e.stopPropagation();
                toggleTools(row, name);
            };
        }
    }
}

/**
 * 渲染单个 server 行 HTML。
 *
 * 参照 ZCode 布局：
 * 左侧：绿灯/红灯 + 名称 + (用户) + 工具数
 * 中间：传输类型 · 命令/URL
 * 右侧：开关 / 编辑 / 删除
 */
function renderServerRow(server, probing) {
    const transport = server.transport || {type: "stdio"};
    const transportType = transport.type || "stdio";
    const transportLabel = transportType === "http" ? "http"
        : transportType === "sse" ? "sse"
            : "stdio";
    const commandDisplay = (transportType === "http" || transportType === "sse")
        ? escapeHtml(transport.url || "")
        : escapeHtml(server.command) + " " + escapeHtml((server.args || []).join(" "));

    const enabledChecked = server.enabled ? "checked" : "";
    const dotClass = !server.enabled
        ? "mcp-status-dot mcp-dot-disabled"
        : probing
            ? "mcp-status-dot mcp-dot-probing"
            : "mcp-status-dot mcp-dot-offline";
    const toolCountText = !server.enabled
        ? "已禁用"
        : probing
            ? "探测中..."
            : "—";

    return `
    <div class="mcp-server-card" data-name="${escapeHtml(server.name)}">
      <div class="mcp-server-header">
        <div class="mcp-server-info">
          <div class="mcp-server-title">
            <span class="${dotClass}"></span>
            <span class="mcp-server-name">${escapeHtml(server.name)}</span>
            <span class="mcp-server-scope">用户</span>
            <span class="mcp-tool-count">${toolCountText}</span>
          </div>
          <div class="mcp-server-transport">
            <span class="mcp-transport-badge">${transportLabel}</span>
            <span class="mcp-server-command">${commandDisplay}</span>
          </div>
          <div class="mcp-error-msg hidden"></div>
        </div>
        <div class="mcp-server-actions">
          <label class="switch switch-sm" title="启用/禁用">
            <input type="checkbox" class="mcp-enabled-cb" ${enabledChecked} />
            <span class="slider"></span>
          </label>
          <button class="btn btn-sm mcp-edit-btn" title="编辑">${iconHTML("pencil")}</button>
          <button class="btn btn-sm mcp-delete-btn" title="删除">${iconHTML("x")}</button>
        </div>
      </div>
      <div class="mcp-server-tools hidden"></div>
    </div>
  `;
}

// ── Server 行事件绑定 ──────────────────────────────────────────────────────────

/**
 * 只刷新列表上方的摘要文字（不重新渲染列表，不重新探测）。
 */
function refreshListSummary() {
    const summaryEl = document.querySelector(".mcp-list-summary");
    if (!summaryEl) return;
    const cards = document.querySelectorAll(".mcp-server-card");
    summaryEl.textContent = `已配置 MCP 服务器 ${cards.length} 项`;
}

/**
 * 为单个 server 行绑定事件。
 */
function bindServerRowEvents(name) {
    const card = document.querySelector(
        `.mcp-server-card[data-name="${cssEscape(name)}"]`
    );
    if (!card) return;

    // enabled 切换
    card.querySelector(".mcp-enabled-cb")?.addEventListener("change", async (e) => {
        e.stopPropagation();
        const enabled = e.target.checked;
        try {
            await invoke("set_mcp_server_enabled", {name, enabled});
            if (enabled) {
                // 启用时重新探测
                probeServer(name);
            } else {
                // 禁用时清理错误信息 + 显示已禁用状态
                updateServerRowStatus(name, "disabled", 0, null, null);
            }
        } catch (err) {
            console.error("set_mcp_server_enabled failed:", err);
            e.target.checked = !e.target.checked;
        }
    });

    // 编辑
    card.querySelector(".mcp-edit-btn")?.addEventListener("click", (e) => {
        e.stopPropagation();
        showEditForm(name);
    });

    // 删除
    card.querySelector(".mcp-delete-btn")?.addEventListener("click", async (e) => {
        e.stopPropagation();
        const ok = await confirmDialog(t("ai.mcp.delete_confirm", {name}), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        try {
            await invoke("delete_mcp_server", {name});
            // 只移除被删除的行，不重新加载整个列表（避免全量重新探测）
            card.remove();
            refreshListSummary();
            // 如果列表空了，显示空提示
            const container = document.getElementById("mcp-server-list");
            if (container && !container.querySelector(".mcp-server-card")) {
                container.innerHTML = '<p class="mcp-empty-hint">' + t("ai.mcp.empty") + '</p>';
            }
        } catch (err) {
            console.error("delete_mcp_server failed:", err);
            messageDialog(`删除失败: ${err}`, {title: t("common.error"), kind: "error"});
        }
    });

    // 点击 header 展开/折叠 tool 列表
    card.querySelector(".mcp-server-header")?.addEventListener("click", (e) => {
        if (e.target.closest("button, input, label, .switch")) return;
        toggleTools(card, name);
    });
}

/**
 * 展开/折叠 tool 列表（或详细报错）。
 */
async function toggleTools(card, name) {
    const toolsDiv = card.querySelector(".mcp-server-tools");
    if (!toolsDiv) return;

    if (!toolsDiv.classList.contains('hidden')) {
        toolsDiv.classList.add('hidden');
        return;
    }

    toolsDiv.classList.remove('hidden');

    // 如果有缓存的错误信息，优先渲染详细报错
    if (card._errorMsg) {
        toolsDiv.innerHTML = `<p class="mcp-error-detail">${escapeHtml(card._errorMsg)}</p>`;
        return;
    }

    // 优先用探测时缓存的 tools
    if (card._cachedTools) {
        renderToolList(toolsDiv, card._cachedTools, name);
        return;
    }

    toolsDiv.innerHTML = '<div class="mcp-tools-loading">加载中...</div>';
    try {
        const tools = await invoke("get_mcp_server_tools", {name});
        renderToolList(toolsDiv, tools, name);
    } catch (e) {
        toolsDiv.innerHTML = `<p class="mcp-error-detail">${escapeHtml(String(e))}</p>`;
    }
}

/**
 * 渲染 tool 列表到容器。
 */
function renderToolList(container, tools, name) {
    if (!tools || tools.length === 0) {
        container.innerHTML = '<p class="mcp-tools-empty">该 server 无 tool</p>';
        return;
    }
    container.innerHTML = tools.map(t => `
    <div class="mcp-tool-item">
      <label class="checkbox">
        <input type="checkbox" class="mcp-tool-toggle" data-tool="${escapeHtml(t.name)}" ${!t.disabled ? "checked" : ""} />
        <span class="checkmark"></span>
      </label>
      <div class="mcp-tool-info">
        <span class="mcp-tool-name">${escapeHtml(t.name)}</span>
        <span class="mcp-tool-desc">${escapeHtml(t.description || "")}</span>
      </div>
    </div>
  `).join("");

    // 绑定 tool toggle
    container.querySelectorAll(".mcp-tool-toggle").forEach(cb => {
        cb.addEventListener("change", async () => {
            const disabledTools = Array.from(
                container.querySelectorAll(".mcp-tool-toggle:not(:checked)")
            ).map(c => c.dataset.tool);
            try {
                await invoke("set_mcp_server_disabled_tools", {name, disabledTools});
            } catch (e) {
                console.error("set_mcp_server_disabled_tools failed:", e);
                cb.checked = !cb.checked;
            }
        });
    });
}

// ── 添加/编辑表单（表单/JSON 双 tab）────────────────────────────────────────────

/**
 * 初始化表单事件。
 */
function initFormHandlers() {
    const addBtn = document.getElementById("mcp-add-server-btn");
    const overlay = document.getElementById("mcp-modal-overlay");
    const saveBtn = document.getElementById("mcp-form-save");
    const cancelBtn = document.getElementById("mcp-form-cancel");
    const transportSelect = document.getElementById("mcp-form-transport");
    const jsonTextarea = document.getElementById("mcp-form-json");

    addBtn?.addEventListener("click", () => showAddForm());
    cancelBtn?.addEventListener("click", () => hideForm());
    saveBtn?.addEventListener("click", () => handleSave());

    transportSelect?.addEventListener("change", () => {
        toggleTransportFields(transportSelect.value);
        syncJsonFromForm();
    });

    // 表单字段变化 → 同步 JSON（防循环）
    ["mcp-form-name", "mcp-form-command", "mcp-form-args", "mcp-form-env",
        "mcp-form-url", "mcp-form-headers", "mcp-form-enabled"].forEach(id => {
        document.getElementById(id)?.addEventListener("input", syncJsonFromForm);
    });

    // JSON 编辑 → 同步表单（防循环，JSON 不完整时静默跳过）
    jsonTextarea?.addEventListener("input", () => {
        syncFormFromJson();
    });

    // 不点击遮罩关闭（防止丢失编辑内容）
    // ESC 关闭
    document.addEventListener("keydown", (e) => {
        if (e.key === "Escape" && overlay && !overlay.classList.contains('hidden')) {
            hideForm();
        }
    });
}

function toggleTransportFields(transport) {
    const stdioFields = document.getElementById("mcp-form-stdio-fields");
    const httpFields = document.getElementById("mcp-form-http-fields");
    // SSE 和 HTTP 都使用 URL + headers 字段
    if (transport === "sse" || transport === "http") {
        if (stdioFields) stdioFields.classList.add('hidden');
        if (httpFields) httpFields.classList.remove('hidden');
    } else {
        if (stdioFields) stdioFields.classList.remove('hidden');
        if (httpFields) httpFields.classList.add('hidden');
    }
}

function showAddForm() {
    const overlay = document.getElementById("mcp-modal-overlay");
    const title = document.getElementById("mcp-form-title");
    const errorEl = document.getElementById("mcp-form-error");
    if (!overlay || !title) return;

    title.textContent = t("ai.mcp.form.title.add");
    document.getElementById("mcp-form-name").value = "";
    document.getElementById("mcp-form-name").disabled = false;
    document.getElementById("mcp-form-transport").value = "stdio";
    document.getElementById("mcp-form-command").value = "";
    document.getElementById("mcp-form-args").value = "";
    document.getElementById("mcp-form-env").value = "";
    document.getElementById("mcp-form-url").value = "";
    document.getElementById("mcp-form-headers").value = "";
    document.getElementById("mcp-form-enabled").checked = true;
    document.getElementById("mcp-form-json").value = "";
    toggleTransportFields("stdio");
    if (errorEl) errorEl.textContent = "";
    overlay.dataset.mode = "add";
    overlay.classList.remove('hidden');
}

async function showEditForm(name) {
    const overlay = document.getElementById("mcp-modal-overlay");
    const title = document.getElementById("mcp-form-title");
    const errorEl = document.getElementById("mcp-form-error");
    if (!overlay || !title) return;

    try {
        const servers = await invoke("list_mcp_servers");
        const server = servers.find(s => s.name === name);
        if (!server) return;

        title.textContent = t("ai.mcp.form.title.edit", {name});
        document.getElementById("mcp-form-name").value = server.name;
        document.getElementById("mcp-form-name").disabled = false;

        const transport = server.transport || {type: "stdio"};
        const transportType = transport.type || "stdio";
        document.getElementById("mcp-form-transport").value = transportType;
        toggleTransportFields(transportType);

        document.getElementById("mcp-form-command").value = server.command || "";
        document.getElementById("mcp-form-args").value = (server.args || []).join(" ");
        document.getElementById("mcp-form-env").value = Object.entries(server.env || {})
            .map(([k, v]) => `${k}=${v}`).join(",");
        document.getElementById("mcp-form-url").value = transport.url || "";
        document.getElementById("mcp-form-headers").value = transport.headers
            ? Object.entries(transport.headers).map(([k, v]) => `${k}: ${v}`).join("\n")
            : "";
        document.getElementById("mcp-form-enabled").checked = server.enabled;

        // 同步 JSON
        syncJsonFromForm();

        if (errorEl) errorEl.textContent = "";
        overlay.dataset.mode = "edit";
        overlay.dataset.originalName = name;
        overlay.classList.remove('hidden');
    } catch (e) {
        console.error("showEditForm failed:", e);
    }
}

function hideForm() {
    const overlay = document.getElementById("mcp-modal-overlay");
    if (overlay) overlay.classList.add('hidden');
}

/**
 * 从表单字段生成 JSON 并同步到 JSON textarea。
 *
 * 输出标准 MCP 配置格式：{ "mcpServers": { "server-name": { ... } } }
 * 与 syncFormFromJson 互逆，确保表单 ↔ JSON 双向无瑕切换。
 */
function syncJsonFromForm() {
    const config = buildJsonFromForm();
    if (!config.name) return;

    // 构建 server 配置体（name 作为 key，不出现在 value 中）
    const serverCfg = {};
    if (config.transport.type === "sse" || config.transport.type === "http") {
        serverCfg.transport = config.transport;
    } else {
        if (config.command) serverCfg.command = config.command;
        if (config.args && config.args.length > 0) serverCfg.args = config.args;
    }
    if (config.env && Object.keys(config.env).length > 0) serverCfg.env = config.env;
    if (!config.enabled) serverCfg.enabled = false;
    if (config.disabled_tools && config.disabled_tools.length > 0) {
        serverCfg.disabled_tools = config.disabled_tools;
    }

    const json = {mcpServers: {}};
    json.mcpServers[config.name] = serverCfg;
    document.getElementById("mcp-form-json").value =
        JSON.stringify(json, null, 2);
}

/**
 * 从 JSON textarea 解析并同步到表单字段。
 *
 * 支持三种格式：
 * - 标准 `{ "mcpServers": { "name": {...} } }`
 * - 简化 `{ "name": {...} }`
 * - 裸配置 `{ "type": "sse", "url": "...", ... }`（用表单中的 name 字段）
 */
function syncFormFromJson() {
    const jsonStr = document.getElementById("mcp-form-json")?.value.trim();
    if (!jsonStr) return;

    try {
        let parsed = JSON.parse(jsonStr);
        if (parsed.mcpServers) parsed = parsed.mcpServers;

        // 判断是否为裸配置（直接是单个 server 的字段，不含 name 键）
        if (isBareServerConfig(parsed)) {
            // 裸配置：保留表单中的 name，只填充其他字段
            const cfg = parsed;
            const rawType = cfg.type || cfg.transport?.type ||
                (cfg.command ? "stdio" : "http");
            const transportType = normalizeTransportType(rawType);
            document.getElementById("mcp-form-transport").value = transportType;
            toggleTransportFields(transportType);

            document.getElementById("mcp-form-command").value = cfg.command || "";
            document.getElementById("mcp-form-args").value = (cfg.args || []).join(" ");
            document.getElementById("mcp-form-env").value = Object.entries(cfg.env || {})
                .map(([k, v]) => `${k}=${v}`).join(",");
            document.getElementById("mcp-form-url").value = cfg.transport?.url || cfg.url || "";
            const headersObj = cfg.transport?.headers || cfg.headers;
            document.getElementById("mcp-form-headers").value = headersObj
                ? Object.entries(headersObj).map(([k, v]) => `${k}: ${v}`).join("\n")
                : "";
            document.getElementById("mcp-form-enabled").checked = cfg.enabled !== false;
            return;
        }

        // 标准/简化格式：取第一个 entry
        const entries = Object.entries(parsed);
        if (entries.length === 0) return;
        const [name, cfg] = entries[0];

        document.getElementById("mcp-form-name").value = name;
        document.getElementById("mcp-form-name").disabled = false;

        const rawType = cfg.transport?.type || cfg.type ||
            (cfg.command ? "stdio" : "http");
        const transportType = normalizeTransportType(rawType);
        document.getElementById("mcp-form-transport").value = transportType;
        toggleTransportFields(transportType);

        document.getElementById("mcp-form-command").value = cfg.command || "";
        document.getElementById("mcp-form-args").value = (cfg.args || []).join(" ");
        document.getElementById("mcp-form-env").value = Object.entries(cfg.env || {})
            .map(([k, v]) => `${k}=${v}`).join(",");
        document.getElementById("mcp-form-url").value = cfg.transport?.url || cfg.url || "";
        const headersObj = cfg.transport?.headers || cfg.headers;
        document.getElementById("mcp-form-headers").value = headersObj
            ? Object.entries(headersObj).map(([k, v]) => `${k}: ${v}`).join("\n")
            : "";
        document.getElementById("mcp-form-enabled").checked = cfg.enabled !== false;
    } catch {
        // JSON 不完整时不报错
    }
}

/**
 * 归一化传输类型：streamable-http → http；sse 保持 sse
 */
function normalizeTransportType(rawType) {
    if (rawType === "streamable-http") return "http";
    if (rawType === "sse" || rawType === "http") return rawType;
    return "stdio";
}

/**
 * 判断 JSON 对象是否为裸 server 配置（不含 name 键，直接是单个 server 的字段）。
 * 裸配置的特征：顶层有 type、command、url 或 transport 字段。
 */
function isBareServerConfig(obj) {
    return obj && typeof obj === 'object' && !Array.isArray(obj) &&
        (obj.type || obj.command || obj.url || obj.transport);
}

/**
 * 从表单字段构建配置对象。
 */
function buildJsonFromForm() {
    const name = document.getElementById("mcp-form-name").value.trim();
    const transportType = document.getElementById("mcp-form-transport").value;
    const enabled = document.getElementById("mcp-form-enabled").checked;

    let transport, command = "", args = [], env = {};

    if (transportType === "sse" || transportType === "http") {
        const url = document.getElementById("mcp-form-url").value.trim();
        const headersStr = document.getElementById("mcp-form-headers").value.trim();
        const headers = {};
        if (headersStr) {
            for (const line of headersStr.split("\n")) {
                const colonIdx = line.indexOf(":");
                if (colonIdx > 0) {
                    const k = line.slice(0, colonIdx).trim();
                    const v = line.slice(colonIdx + 1).trim();
                    if (k) headers[k] = v;
                }
            }
        }
        transport = {type: transportType, url, headers};
    } else {
        command = document.getElementById("mcp-form-command").value.trim();
        const argsStr = document.getElementById("mcp-form-args").value.trim();
        args = argsStr ? argsStr.split(/\s+/).filter(Boolean) : [];
        const envStr = document.getElementById("mcp-form-env").value.trim();
        if (envStr) {
            for (const pair of envStr.split(",")) {
                const eqIdx = pair.indexOf("=");
                if (eqIdx > 0) {
                    env[pair.slice(0, eqIdx).trim()] = pair.slice(eqIdx + 1).trim();
                }
            }
        }
        transport = {type: "stdio"};
    }

    // 使用 snake_case 与后端 Rust struct 字段名一致
    return {name, transport, command, args, env, enabled, disabled_tools: []};
}

/**
 * 处理保存（添加或编辑）——表单和 JSON 实时同步，直接用表单字段构建配置。
 */
async function handleSave() {
    const overlay = document.getElementById("mcp-modal-overlay");
    const errorEl = document.getElementById("mcp-form-error");
    if (!overlay) return;

    // 表单和 JSON 实时同步，直接用表单字段构建配置
    let config = buildJsonFromForm();
    if (!config.name) {
        if (errorEl) errorEl.textContent = t("ai.mcp.form_err.empty");
        return;
    }
    // 通用校验：HTTP/SSE 模式需要 URL
    if ((config.transport.type === "http" || config.transport.type === "sse")
        && !config.transport.url) {
        if (errorEl) errorEl.textContent = "HTTP/SSE 模式需要填写 URL";
        return;
    }
    // 通用校验：stdio 模式需要 command
    if (config.transport.type === "stdio" && !config.command) {
        if (errorEl) errorEl.textContent = t("ai.mcp.form_err.empty");
        return;
    }

    try {
        // 编辑模式下，如果名称变了，先删除旧名称的配置
        const originalName = overlay.dataset.originalName;
        const renamed =
            overlay.dataset.mode === "edit" && originalName && originalName !== config.name;
        if (renamed) {
            await invoke("delete_mcp_server", {name: originalName});
            // 同步移除旧 name 的 DOM 行（upsertServerRow 按新 name 查找，不会命中旧行）
            const oldRow = document.querySelector(
                `.mcp-server-card[data-name="${cssEscape(originalName)}"]`
            );
            if (oldRow) oldRow.remove();
        }
        await invoke("upsert_mcp_server", {config});
        hideForm();
        // 0.13.7: 局部更新该行，避免全量重新探测（不再调用 loadServerList）
        // renamed 时旧行已移除 → 走新增分支；否则走编辑分支原地替换
        await upsertServerRow(config);
    } catch (e) {
        console.error("handleSave failed:", e);
        if (errorEl) errorEl.textContent = String(e);
    }
}

// ── 辅助函数 ───────────────────────────────────────────────────────────────────

function escapeHtml(s) {
    if (s == null) return "";
    return String(s)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;")
        .replace(/'/g, "&#39;");
}

/** CSS.escape polyfill for server name selectors。 */
function cssEscape(s) {
    if (window.CSS && CSS.escape) return CSS.escape(s);
    // 简单 fallback：转义引号
    return String(s).replace(/"/g, '\\"');
}

// ── MCP 导入（0.13.6）──────────────────────────────────────────────────────────

let importPreviewData = null;
let existingServerNames = new Set();

function initImportHandlers() {
    const importBtn = document.getElementById("mcp-import-btn");
    const overlay = document.getElementById("mcp-import-overlay");
    const sourceSelect = document.getElementById("mcp-import-source");
    const confirmBtn = document.getElementById("mcp-import-confirm");
    const cancelBtn = document.getElementById("mcp-import-cancel");

    importBtn?.addEventListener("click", () => showImportForm());
    cancelBtn?.addEventListener("click", () => hideImportForm());
    confirmBtn?.addEventListener("click", () => handleImportConfirm());

    // 选择来源 → 自动读取
    sourceSelect?.addEventListener("change", () => {
        const source = sourceSelect.value;
        if (source) {
            handleImportLoad();
        } else {
            resetImportList();
        }
    });

    // 全选/全不选
    document.getElementById("mcp-import-select-all")?.addEventListener("change", (e) => {
        const checked = e.target.checked;
        document.querySelectorAll(".mcp-import-select").forEach(cb => {
            cb.checked = checked;
        });
        updateConfirmButtonState();
    });

    // 单个 checkbox 变化时同步全选框状态
    document.getElementById("mcp-import-preview-list")?.addEventListener("change", (e) => {
        if (!e.target.classList.contains("mcp-import-select")) return;
        const allCbs = document.querySelectorAll(".mcp-import-select");
        const allChecked = Array.from(allCbs).every(cb => cb.checked);
        const selectAll = document.getElementById("mcp-import-select-all");
        if (selectAll) selectAll.checked = allChecked;
        updateConfirmButtonState();
    });

    overlay?.addEventListener("click", (e) => {
        if (e.target === overlay) hideImportForm();
    });
}

async function showImportForm() {
    const overlay = document.getElementById("mcp-import-overlay");
    if (!overlay) return;

    document.getElementById("mcp-import-source").value = "";
    document.getElementById("mcp-import-overwrite").checked = false;
    resetImportList();
    const errorEl = document.getElementById("mcp-import-error");
    if (errorEl) errorEl.textContent = "";

    try {
        const servers = await invoke("list_mcp_servers");
        existingServerNames = new Set(servers.map(s => s.name));
    } catch {
        existingServerNames = new Set();
    }

    overlay.classList.remove('hidden');
}

function hideImportForm() {
    const overlay = document.getElementById("mcp-import-overlay");
    if (overlay) overlay.classList.add('hidden');
    importPreviewData = null;
}

function resetImportList() {
    const listDiv = document.getElementById("mcp-import-preview-list");
    const countEl = document.getElementById("mcp-import-count");
    const confirmBtn = document.getElementById("mcp-import-confirm");
    const selectAll = document.getElementById("mcp-import-select-all");
    if (listDiv) listDiv.innerHTML = '<div class="mcp-import-empty">选择导入来源后将自动读取配置</div>';
    if (countEl) countEl.textContent = "";
    if (confirmBtn) confirmBtn.disabled = true;
    if (selectAll) {
        selectAll.checked = true;
        selectAll.disabled = true;
    }
    importPreviewData = null;
}

function updateConfirmButtonState() {
    const confirmBtn = document.getElementById("mcp-import-confirm");
    if (!confirmBtn) return;
    const checked = document.querySelectorAll(".mcp-import-select:checked");
    confirmBtn.disabled = checked.length === 0;
}

async function handleImportLoad() {
    const source = document.getElementById("mcp-import-source").value;
    const errorEl = document.getElementById("mcp-import-error");
    if (errorEl) errorEl.textContent = "";

    if (!source) {
        if (errorEl) errorEl.textContent = "请选择导入来源";
        return;
    }

    // 优雅显示加载状态：不替换整个 innerHTML，只更新已有 empty 元素的文本，避免布局抖动
    const listDiv = document.getElementById("mcp-import-preview-list");
    if (listDiv) {
        listDiv.classList.add("loading");
        const emptyEl = listDiv.querySelector(".mcp-import-empty");
        if (emptyEl) {
            emptyEl.textContent = "正在读取…";
        } else {
            // 列表已有内容（如上次导入预览），先不替换，保持稳定
            // 仅添加 loading class 让 CSS 处理半透明效果
        }
    }
    const countEl = document.getElementById("mcp-import-count");
    if (countEl) countEl.textContent = "";
    // 禁用全选和确认按钮
    const selectAllLoading = document.getElementById("mcp-import-select-all");
    if (selectAllLoading) {
        selectAllLoading.disabled = true;
        selectAllLoading.checked = true;
    }
    const confirmBtnLoading = document.getElementById("mcp-import-confirm");
    if (confirmBtnLoading) confirmBtnLoading.disabled = true;

    let configs;
    try {
        configs = await invoke("import_mcp_from_agent", {source});
    } catch (e) {
        if (errorEl) errorEl.textContent = String(e);
        if (listDiv) listDiv.classList.remove("loading");
        return;
    }

    if (listDiv) listDiv.classList.remove("loading");

    if (!configs || configs.length === 0) {
        if (listDiv) listDiv.innerHTML = '<div class="mcp-import-empty">未找到可导入的 server 配置</div>';
        const confirmBtn0 = document.getElementById("mcp-import-confirm");
        if (confirmBtn0) confirmBtn0.disabled = true;
        if (errorEl) errorEl.textContent = "未找到可导入的 server 配置";
        return;
    }

    importPreviewData = configs;
    renderImportPreview(configs);
}

function renderImportPreview(configs) {
    const listDiv = document.getElementById("mcp-import-preview-list");
    if (!listDiv) return;

    listDiv.innerHTML = configs.map((c, i) => {
        const exists = existingServerNames.has(c.name);
        const badge = exists
            ? '<span class="mcp-import-badge mcp-import-badge-exists">已存在</span>'
            : '<span class="mcp-import-badge mcp-import-badge-new">新增</span>';
        const transport = c.transport || {type: "stdio"};
        const transportType = transport.type || "stdio";
        const transportLabel = transportType === "http" ? "http"
            : transportType === "sse" ? "sse"
                : "stdio";
        const cmdDisplay = (transportType === "http" || transportType === "sse")
            ? escapeHtml(transport.url || "")
            : escapeHtml(c.command || "") + " " + escapeHtml((c.args || []).join(" "));
        return `
      <div class="mcp-import-item" data-index="${i}">
        <label class="checkbox">
          <input type="checkbox" class="mcp-import-select" data-index="${i}" checked />
          <span class="checkmark"></span>
        </label>
        <div class="mcp-import-info">
          <div class="mcp-import-row">
            <span class="mcp-import-name">${escapeHtml(c.name)}</span>
            ${badge}
            <span class="mcp-transport-badge">${transportLabel}</span>
          </div>
          <span class="mcp-import-command">${cmdDisplay}</span>
        </div>
      </div>
    `;
    }).join("");

    // 更新计数与按钮状态
    const countEl = document.getElementById("mcp-import-count");
    if (countEl) countEl.textContent = `${configs.length} 项`;
    const selectAll = document.getElementById("mcp-import-select-all");
    if (selectAll) selectAll.disabled = false;
    updateConfirmButtonState();
}

async function handleImportConfirm() {
    if (!importPreviewData) return;
    const errorEl = document.getElementById("mcp-import-error");
    const overwrite = document.getElementById("mcp-import-overwrite").checked;

    // 只导入用户勾选的配置
    const selectedIndices = Array.from(
        document.querySelectorAll(".mcp-import-select:checked")
    ).map(cb => parseInt(cb.dataset.index, 10));
    const selectedConfigs = importPreviewData.filter((_, i) =>
        selectedIndices.includes(i));

    if (selectedConfigs.length === 0) {
        if (errorEl) errorEl.textContent = "请至少选择一个 server 导入";
        return;
    }

    try {
        const result = await invoke("batch_import_mcp_servers", {
            configs: selectedConfigs,
            overwrite,
        });
        hideImportForm();
        // 0.13.7: 局部追加/更新被导入的行，避免全量重新探测（不再调用 loadServerList）。
        // overwrite=true 且 name 已存在时走 upsertServerRow 的编辑分支（原地替换+重探测）。
        for (const cfg of selectedConfigs) {
            // 跳过被跳过（未覆盖）的项，避免无谓探测
            if (overwrite === false && result.names && !result.names.includes(cfg.name)) {
                continue;
            }
            await upsertServerRow(cfg);
        }
        const msg = `导入完成：${result.imported} 新增，${result.overwritten} 覆盖，${result.skipped} 跳过`;
        console.log(msg);
    } catch (e) {
        if (errorEl) errorEl.textContent = String(e);
    }
}
