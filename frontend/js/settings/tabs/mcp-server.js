/**
 * MCP Server Tab 模块
 *
 * 管理 Blink 作为 MCP server 的配置：总开关 + 端口 + 简明状态 + 目录深链。
 * 详细能力管理已迁移至"能力与操作"页。
 *
 * 后端 commands:
 * - get_mcp_server_config — 读取配置（enabled + port + exposed_capabilities）
 * - set_mcp_server_config — 保存配置
 * - get_mcp_server_runtime_status — 获取运行时状态快照（status / endpoint / port / tool_count / error）
 * - get_catalog_mcp_summary — 获取 MCP 能力摘要（exposed_count / total_count）
 */
import { invoke, listen } from "../../shared/tauri.js";
import { EVENTS } from "../../shared/event-names.js";
import { t } from "../../i18n/index.js";

/**
 * 初始化 MCP Server 配置区。
 */
export async function initMcpServerSection() {
  await loadConfig();
  initToggle();
  initPortInput();
  initCopyConfig();
  initCapabilitiesLink();
  startStatusPolling();

  // 能力目录页改暴露状态后（set_mcp_server_config 广播）刷新 N/M 摘要
  listen(EVENTS.CONFIG_CHANGED, () => {
    updateCapabilitiesSummary();
  });
}

// ── 配置加载与渲染 ────────────────────────────────────────────────────────────

let currentConfig = { enabled: false, port: 32123, exposed_capabilities: [] };

async function loadConfig() {
  try {
    currentConfig = await invoke("get_mcp_server_config");
    updateToggle();
    updatePortInput();
    updateDetailVisibility();
    updateConfigJson();
    await updateCapabilitiesSummary();
  } catch (e) {
    console.error("loadMcpServerConfig failed:", e);
  }
}

async function updateCapabilitiesSummary() {
  const container = document.getElementById("mcp-server-capabilities-summary");
  if (!container) return;

  try {
    const summary = await invoke("get_catalog_mcp_summary");
    const exposedCount = summary.exposed_count || 0;
    const totalCount = summary.total_count || 0;

    container.innerHTML = `
      <div class="mcp-summary">
        <span class="mcp-summary-text">
          ${t("ai.mcp_server.exposed_summary", { exposed_count: exposedCount, total_count: totalCount })}
        </span>
        <button class="btn-link btn-small" id="mcp-view-capabilities">
          ${t("ai.mcp_server.view_capabilities")}
        </button>
      </div>
    `;
  } catch (e) {
    container.innerHTML = '<p class="mcp-error">加载失败: ' + escapeHtml(String(e)) + "</p>";
    console.error("updateCapabilitiesSummary failed:", e);
  }
}

function initCapabilitiesLink() {
  document.addEventListener("click", (e) => {
    if (e.target && e.target.id === "mcp-view-capabilities") {
      // 跳转到能力与操作页，过滤到 MCP 出口列并滚动到控制区
      document.querySelector('[data-tab="capabilities"]')?.click();
      setTimeout(() => {
        const exitFilter = document.getElementById("filter-exit");
        if (exitFilter) {
          exitFilter.value = "mcp";
          exitFilter.dispatchEvent(new Event("change"));
        }
        document
          .getElementById("capabilities-container")
          ?.scrollIntoView({ behavior: "smooth", block: "start" });
      }, 100);
    }
  });
}

// ── 总开关 ────────────────────────────────────────────────────────────────────

function initToggle() {
  const toggle = document.getElementById("mcp-server-enabled-toggle");
  if (!toggle) return;

  toggle.addEventListener("change", async () => {
    currentConfig.enabled = toggle.checked;
    await saveConfig();
    updateDetailVisibility();
  });
}

function updateToggle() {
  const toggle = document.getElementById("mcp-server-enabled-toggle");
  if (toggle) {
    toggle.checked = currentConfig.enabled;
  }
}

// ── 端口输入 ──────────────────────────────────────────────────────────────────

function initPortInput() {
  const input = document.getElementById("mcp-server-port-input");
  if (!input) return;

  input.addEventListener("change", async () => {
    const port = parseInt(input.value, 10);
    if (isNaN(port) || port < 1024 || port > 65535) {
      alert(t("ai.mcp_server.port_invalid") || "端口必须在 1024–65535 范围内");
      input.value = currentConfig.port || 32123;
      return;
    }
    currentConfig.port = port;
    await saveConfig();
    updateConfigJson();
  });
}

function updatePortInput() {
  const input = document.getElementById("mcp-server-port-input");
  if (input) {
    input.value = currentConfig.port || 32123;
  }
}

/**
 * 根据开关状态显示/隐藏能力列表和使用方式。
 */
function updateDetailVisibility() {
  const detail = document.getElementById("mcp-server-detail");
  if (detail) {
    detail.classList.toggle('hidden', !currentConfig.enabled);
  }
}

// ── 运行时状态轮询 ────────────────────────────────────────────────────────────

let statusPollTimer = null;

function startStatusPolling() {
  // 立即查询一次
  refreshStatus();
  // 每 3 秒轮询一次运行时状态
  if (statusPollTimer) clearInterval(statusPollTimer);
  statusPollTimer = setInterval(refreshStatus, 3000);
}

async function refreshStatus() {
  try {
    const snapshot = await invoke("get_mcp_server_runtime_status");
    renderStatus(snapshot);
  } catch (e) {
    console.error("getMcpServerRuntimeStatus failed:", e);
  }
}

function renderStatus(snapshot) {
  const dot = document.getElementById("mcp-server-status-dot");
  const text = document.getElementById("mcp-server-status-text");
  const endpoint = document.getElementById("mcp-server-endpoint");
  const toolCount = document.getElementById("mcp-server-tool-count");
  const errorMsg = document.getElementById("mcp-server-error-msg");

  if (!dot || !text) return;

  // 状态映射
  const statusMap = {
    disabled: { class: "mcp-dot-disabled", label: t("ai.mcp_server.status.disabled") || "未启用" },
    starting: { class: "mcp-dot-probing", label: t("ai.mcp_server.status.starting") || "启动中" },
    listening: { class: "mcp-dot-online", label: t("ai.mcp_server.status.listening") || "运行中" },
    error: { class: "mcp-dot-offline", label: t("ai.mcp_server.status.error") || "错误" },
  };

  const info = statusMap[snapshot.status] || statusMap.disabled;

  // 更新状态点
  dot.className = "mcp-runtime-dot " + info.class;

  // 更新状态文字
  text.textContent = info.label;

  // 更新 endpoint
  if (endpoint) {
    if (snapshot.endpoint) {
      endpoint.textContent = snapshot.endpoint;
      endpoint.style.display = "";
    } else {
      endpoint.textContent = "";
      endpoint.style.display = "none";
    }
  }

  // 更新 tool 数量
  if (toolCount) {
    if (snapshot.status === "listening" && snapshot.tool_count > 0) {
      toolCount.textContent = `${snapshot.tool_count} ${t("ai.mcp_server.tools") || "个工具"}`;
      toolCount.style.display = "";
    } else {
      toolCount.textContent = "";
      toolCount.style.display = "none";
    }
  }

  // 更新错误信息
  if (errorMsg) {
    if (snapshot.error) {
      errorMsg.textContent = snapshot.error;
      errorMsg.style.display = "";
    } else {
      errorMsg.textContent = "";
      errorMsg.style.display = "none";
    }
  }
}

// ── 使用方式：配置 JSON + 复制 ────────────────────────────────────────────────

/**
 * 生成 MCP 客户端配置 JSON 并填充到 <pre> 中。
 * 0.19.13: 使用 Streamable HTTP URL 格式。
 */
function updateConfigJson() {
  const pre = document.getElementById("mcp-server-config-json");
  if (!pre) return;

  const port = currentConfig.port || 32123;
  const config = {
    mcpServers: {
      blink: {
        url: `http://127.0.0.1:${port}/mcp`,
      },
    },
  };
  pre.textContent = JSON.stringify(config, null, 2);
}

/**
 * 初始化复制配置按钮。
 */
function initCopyConfig() {
  const btn = document.getElementById("mcp-copy-config-btn");
  if (!btn) return;

  btn.addEventListener("click", async () => {
    const pre = document.getElementById("mcp-server-config-json");
    if (!pre) return;
    const text = pre.textContent || "";
    try {
      await navigator.clipboard.writeText(text);
      // 临时反馈
      const span = btn.querySelector("span");
      if (span) {
        const original = span.textContent;
        span.textContent = t("ai.mcp_server.copy_success");
        setTimeout(() => { span.textContent = original; }, 2000);
      }
    } catch (e) {
      console.error("clipboard write failed:", e);
    }
  });
}

// ── 保存配置 ──────────────────────────────────────────────────────────────────

async function saveConfig() {
  try {
    await invoke("set_mcp_server_config", { config: currentConfig });
    // 保存后立即刷新状态（启停/端口变更会触发 runtime 状态变化）
    setTimeout(refreshStatus, 200);
  } catch (e) {
    console.error("saveMcpServerConfig failed:", e);
    alert("保存 MCP server 配置失败: " + e);
    // 重新加载以回滚 UI
    await loadConfig();
  }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

function escapeHtml(s) {
  if (s == null) return "";
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}
