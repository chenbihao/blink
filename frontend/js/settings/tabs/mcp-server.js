/**
 * MCP Server Tab 模块
 *
 * 管理 Blink 作为 MCP server 的配置：总开关 + 端口 + 暴露能力清单 + 运行时状态 + 使用方式。
 *
 * 后端 commands:
 * - get_mcp_server_config — 读取配置（enabled + port + exposed_capabilities）
 * - set_mcp_server_config — 保存配置
 * - list_exposable_capabilities — 列出所有可暴露的 Capability
 * - get_mcp_server_runtime_status — 获取运行时状态快照（status / endpoint / port / tool_count / error）
 */
import { invoke } from "../../shared/tauri.js";
import { t } from "../../i18n/index.js";

/**
 * 初始化 MCP Server 配置区。
 */
export async function initMcpServerSection() {
  await loadConfig();
  initToggle();
  initPortInput();
  initCopyConfig();
  startStatusPolling();
}

// ── 配置加载与渲染 ────────────────────────────────────────────────────────────

let currentConfig = { enabled: false, port: 32123, exposed_capabilities: [] };

async function loadConfig() {
  try {
    currentConfig = await invoke("get_mcp_server_config");
    await renderCapabilities();
    updateToggle();
    updatePortInput();
    updateDetailVisibility();
    updateConfigJson();
  } catch (e) {
    console.error("loadMcpServerConfig failed:", e);
  }
}

async function renderCapabilities() {
  const container = document.getElementById("mcp-server-capabilities");
  if (!container) return;

  try {
    const caps = await invoke("list_exposable_capabilities");
    if (!caps || caps.length === 0) {
      container.innerHTML =
        '<p class="mcp-empty-hint">无可用 Capability</p>';
      return;
    }

    container.innerHTML = caps
      .map((cap) => {
        const checked = currentConfig.exposed_capabilities.includes(cap.id)
          ? "checked"
          : "";
        const sensitiveTag = cap.sensitive
          ? '<span class="mcp-sensitive-tag">sensitive</span>'
          : "";
        return `
      <div class="mcp-capability-item">
        <label class="checkbox">
          <input type="checkbox" class="mcp-cap-toggle" data-cap-id="${escapeHtml(cap.id)}" ${checked} />
          <span class="checkmark"></span>
        </label>
        <div class="mcp-capability-info">
          <span class="mcp-capability-name">${escapeHtml(cap.id)} ${sensitiveTag}</span>
          <span class="mcp-capability-desc">${escapeHtml(cap.description || "")}</span>
        </div>
      </div>
    `;
      })
      .join("");

    // 绑定 checkbox 事件
    container.querySelectorAll(".mcp-cap-toggle").forEach((cb) => {
      cb.addEventListener("change", async () => {
        const capId = cb.dataset.capId;
        if (cb.checked) {
          if (!currentConfig.exposed_capabilities.includes(capId)) {
            currentConfig.exposed_capabilities.push(capId);
          }
        } else {
          currentConfig.exposed_capabilities =
            currentConfig.exposed_capabilities.filter((c) => c !== capId);
        }
        await saveConfig();
      });
    });
  } catch (e) {
    container.innerHTML =
      '<p class="mcp-error">加载失败: ' + escapeHtml(String(e)) + "</p>";
    console.error("renderCapabilities failed:", e);
  }
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
