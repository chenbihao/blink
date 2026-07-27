/**
 * MCP Tab 模块（0.13.0）
 *
 * 管理 MCP server 配置：列表 / 添加 / 编辑 / 删除 / 启停 / 重连 / tool 预览 / 粒度开关。
 *
 * 后端 commands:
 * - list_mcp_servers — 列出所有 server（含状态）
 * - upsert_mcp_server — 添加/更新 server 配置
 * - delete_mcp_server — 删除 server（同时停止）
 * - set_mcp_server_enabled — 切换 enabled
 * - start_mcp_server — 手动启动
 * - stop_mcp_server — 手动停止
 * - reconnect_mcp_server — 重连
 * - get_mcp_server_tools — 获取 tool 列表
 * - set_mcp_server_disabled_tools — 更新 tool 可见性
 * - get_mcp_tool_pool_size — 获取 tool 池规模
 */
import { invoke } from "../../tauri.js";
import { t } from "../../i18n/index.js";

/**
 * 初始化 MCP Tab
 */
export function initMcPTab() {
  loadServerList();
  initFormHandlers();
}

// ── Server 列表加载与渲染 ──────────────────────────────────────────────────────

/**
 * 加载并渲染 server 列表。
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
    container.innerHTML = servers.map(renderServerCard).join("");
    // 绑定每个 server 卡片的事件
    servers.forEach((s) => bindServerCardEvents(s.name));
  } catch (e) {
    container.innerHTML = '<p class="mcp-error">' + t("ai.mcp.load_failed") + ': ' + escapeHtml(e) + '</p>';
    console.error("loadServerList failed:", e);
  }
}

/**
 * 渲染单个 server 卡片 HTML。
 */
function renderServerCard(server) {
  const statusBadge = renderStatusBadge(server.status);
  const toolCount = getToolCount(server.status);
  const enabledChecked = server.enabled ? "checked" : "";

  return `
    <div class="mcp-server-card" data-name="${escapeHtml(server.name)}">
      <div class="mcp-server-header">
        <div class="mcp-server-info">
          <span class="mcp-server-name">${escapeHtml(server.name)}</span>
          <span class="mcp-server-command">${escapeHtml(server.command)} ${escapeHtml((server.args || []).join(" "))}</span>
        </div>
        <div class="mcp-server-actions">
          ${statusBadge}
          <label class="checkbox mcp-enabled-toggle" title="自动启动">
            <input type="checkbox" class="mcp-enabled-cb" ${enabledChecked} />
            <span class="checkmark"></span>
          </label>
          <button class="btn btn-sm mcp-start-btn" title="启动">▶</button>
          <button class="btn btn-sm mcp-stop-btn" title="停止">■</button>
          <button class="btn btn-sm mcp-reconnect-btn" title="重连">↻</button>
          <button class="btn btn-sm mcp-edit-btn" title="编辑">✎</button>
          <button class="btn btn-sm mcp-delete-btn" title="删除">✕</button>
        </div>
      </div>
      <div class="mcp-server-tools" style="display:none;">
        <div class="mcp-tools-loading">加载中...</div>
      </div>
    </div>
  `;
}

/**
 * 渲染状态徽章。
 */
function renderStatusBadge(status) {
  if (!status) return '<span class="mcp-status mcp-status-offline">' + t("ai.mcp.status.offline") + '</span>';
  if (status.online !== undefined) {
    return `<span class="mcp-status mcp-status-online">在线 · ${status.online.tool_count} tools</span>`;
  }
  if (status.offline !== undefined) {
    return `<span class="mcp-status mcp-status-offline" title="${escapeHtml(status.offline.reason || "")}">离线</span>`;
  }
  if (status.connecting !== undefined) {
    return '<span class="mcp-status mcp-status-connecting">连接中...</span>';
  }
  return '<span class="mcp-status mcp-status-offline">未知</span>';
}

/**
 * 从状态对象提取 tool 数量。
 */
function getToolCount(status) {
  if (status && status.online !== undefined) {
    return status.online.tool_count;
  }
  return 0;
}

// ── Server 卡片事件绑定 ────────────────────────────────────────────────────────

/**
 * 为单个 server 卡片绑定事件。
 */
function bindServerCardEvents(name) {
  const card = document.querySelector(
    `.mcp-server-card[data-name="${cssEscape(name)}"]`
  );
  if (!card) return;

  // enabled 切换
  const enabledCb = card.querySelector(".mcp-enabled-cb");
  enabledCb?.addEventListener("change", async () => {
    try {
      await invoke("set_mcp_server_enabled", { name, enabled: enabledCb.checked });
    } catch (e) {
      console.error("set_mcp_server_enabled failed:", e);
      enabledCb.checked = !enabledCb.checked; // 回滚
    }
  });

  // 启动
  card.querySelector(".mcp-start-btn")?.addEventListener("click", async () => {
    try {
      await invoke("start_mcp_server", { name });
      await loadServerList();
    } catch (e) {
      console.error("start_mcp_server failed:", e);
      alert(`启动失败: ${e}`);
    }
  });

  // 停止
  card.querySelector(".mcp-stop-btn")?.addEventListener("click", async () => {
    try {
      await invoke("stop_mcp_server", { name });
      await loadServerList();
    } catch (e) {
      console.error("stop_mcp_server failed:", e);
    }
  });

  // 重连
  card.querySelector(".mcp-reconnect-btn")?.addEventListener("click", async () => {
    try {
      await invoke("reconnect_mcp_server", { name });
      await loadServerList();
    } catch (e) {
      console.error("reconnect_mcp_server failed:", e);
      alert(`重连失败: ${e}`);
    }
  });

  // 编辑
  card.querySelector(".mcp-edit-btn")?.addEventListener("click", () => {
    showEditForm(name);
  });

  // 删除
  card.querySelector(".mcp-delete-btn")?.addEventListener("click", async () => {
    if (!confirm(t("ai.mcp.delete_confirm", { name }))) return;
    try {
      await invoke("delete_mcp_server", { name });
      await loadServerList();
    } catch (e) {
      console.error("delete_mcp_server failed:", e);
      alert(`删除失败: ${e}`);
    }
  });

  // 点击 header 展开/折叠 tool 列表
  const header = card.querySelector(".mcp-server-header");
  header?.addEventListener("click", (e) => {
    // 不响应按钮区域的点击
    if (e.target.closest("button, input, label")) return;
    toggleTools(card, name);
  });
}

/**
 * 展开/折叠 tool 列表。
 */
async function toggleTools(card, name) {
  const toolsDiv = card.querySelector(".mcp-server-tools");
  if (!toolsDiv) return;

  if (toolsDiv.style.display !== "none") {
    toolsDiv.style.display = "none";
    return;
  }

  toolsDiv.style.display = "block";
  toolsDiv.innerHTML = '<div class="mcp-tools-loading">加载中...</div>';

  try {
    const tools = await invoke("get_mcp_server_tools", { name });
    if (!tools || tools.length === 0) {
      toolsDiv.innerHTML = '<p class="mcp-tools-empty">该 server 无 tool</p>';
      return;
    }
    toolsDiv.innerHTML = tools
      .map(
        (t) => `
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
    `
      )
      .join("");

    // 绑定 tool toggle 事件
    toolsDiv.querySelectorAll(".mcp-tool-toggle").forEach((cb) => {
      cb.addEventListener("change", async () => {
        // 收集所有未勾选的 tool 名称
        const disabledTools = Array.from(
          toolsDiv.querySelectorAll(".mcp-tool-toggle:not(:checked)")
        ).map((c) => c.dataset.tool);

        try {
          await invoke("set_mcp_server_disabled_tools", {
            name,
            disabledTools,
          });
        } catch (e) {
          console.error("set_mcp_server_disabled_tools failed:", e);
          cb.checked = !cb.checked; // 回滚
        }
      });
    });
  } catch (e) {
    toolsDiv.innerHTML = `<p class="mcp-error">加载失败: ${escapeHtml(e)}</p>`;
  }
}

// ── 添加/编辑表单 ──────────────────────────────────────────────────────────────

/**
 * 初始化表单事件（添加按钮、保存、取消）。
 */
function initFormHandlers() {
  const addBtn = document.getElementById("mcp-add-server-btn");
  const overlay = document.getElementById("mcp-modal-overlay");
  const saveBtn = document.getElementById("mcp-form-save");
  const cancelBtn = document.getElementById("mcp-form-cancel");

  addBtn?.addEventListener("click", () => showAddForm());
  cancelBtn?.addEventListener("click", () => hideForm());
  saveBtn?.addEventListener("click", () => handleSave());

  // 点击遮罩层关闭弹窗（点击内容区不关闭）
  overlay?.addEventListener("click", (e) => {
    if (e.target === overlay) hideForm();
  });

  // ESC 关闭弹窗
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && overlay && overlay.style.display !== "none") {
      hideForm();
    }
  });
}

/**
 * 显示添加表单。
 */
function showAddForm() {
  const overlay = document.getElementById("mcp-modal-overlay");
  const title = document.getElementById("mcp-form-title");
  const errorEl = document.getElementById("mcp-form-error");
  if (!overlay || !title) return;

  title.textContent = t("ai.mcp.form.title.add");
  document.getElementById("mcp-form-name").value = "";
  document.getElementById("mcp-form-name").disabled = false;
  document.getElementById("mcp-form-command").value = "";
  document.getElementById("mcp-form-args").value = "";
  document.getElementById("mcp-form-env").value = "";
  document.getElementById("mcp-form-enabled").checked = true;
  if (errorEl) errorEl.textContent = "";
  overlay.dataset.mode = "add";
  overlay.style.display = "";
}

/**
 * 显示编辑表单（预填已有配置）。
 */
async function showEditForm(name) {
  const overlay = document.getElementById("mcp-modal-overlay");
  const title = document.getElementById("mcp-form-title");
  const errorEl = document.getElementById("mcp-form-error");
  if (!overlay || !title) return;

  try {
    const servers = await invoke("list_mcp_servers");
    const server = servers.find((s) => s.name === name);
    if (!server) return;

    title.textContent = t("ai.mcp.form.title.edit", { name });
    document.getElementById("mcp-form-name").value = server.name;
    document.getElementById("mcp-form-name").disabled = true; // name 不可改
    document.getElementById("mcp-form-command").value = server.command || "";
    document.getElementById("mcp-form-args").value = (server.args || []).join(" ");
    document.getElementById("mcp-form-env").value = Object.entries(server.env || {})
      .map(([k, v]) => `${k}=${v}`)
      .join(",");
    document.getElementById("mcp-form-enabled").checked = server.enabled;
    if (errorEl) errorEl.textContent = "";
    overlay.dataset.mode = "edit";
    overlay.style.display = "";
  } catch (e) {
    console.error("showEditForm failed:", e);
  }
}

/**
 * 隐藏表单。
 */
function hideForm() {
  const overlay = document.getElementById("mcp-modal-overlay");
  if (overlay) overlay.style.display = "none";
}

/**
 * 处理保存（添加或编辑）。
 */
async function handleSave() {
  const overlay = document.getElementById("mcp-modal-overlay");
  const errorEl = document.getElementById("mcp-form-error");
  if (!overlay) return;

  const name = document.getElementById("mcp-form-name").value.trim();
  const command = document.getElementById("mcp-form-command").value.trim();
  const argsStr = document.getElementById("mcp-form-args").value.trim();
  const envStr = document.getElementById("mcp-form-env").value.trim();
  const enabled = document.getElementById("mcp-form-enabled").checked;

  if (!name || !command) {
    if (errorEl) errorEl.textContent = t("ai.mcp.form_err.empty");
    return;
  }

  // 解析参数（空格分隔）
  const args = argsStr ? argsStr.split(/\s+/).filter(Boolean) : [];

  // 解析环境变量（KEY=value 逗号分隔）
  const env = {};
  if (envStr) {
    for (const pair of envStr.split(",")) {
      const eqIdx = pair.indexOf("=");
      if (eqIdx > 0) {
        env[pair.slice(0, eqIdx).trim()] = pair.slice(eqIdx + 1).trim();
      }
    }
  }

  try {
    await invoke("upsert_mcp_server", {
      config: { name, command, args, env, enabled, disabledTools: [] },
    });
    hideForm();
    await loadServerList();
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

function cssEscape(s) {
  return String(s).replace(/"/g, '\\"');
}
