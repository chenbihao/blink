/**
 * MCP Server Tab 模块
 *
 * 管理 Blink 作为 MCP server 的配置：总开关 + 暴露能力清单 + 使用方式。
 *
 * 后端 commands:
 * - get_mcp_server_config — 读取配置（enabled + exposed_capabilities）
 * - set_mcp_server_config — 保存配置
 * - list_exposable_capabilities — 列出所有可暴露的 Capability
 */
import { invoke } from "../../shared/tauri.js";
import { t } from "../../i18n/index.js";

/**
 * 初始化 MCP Server 配置区。
 */
export async function initMcpServerSection() {
  await loadConfig();
  initToggle();
  initCopyConfig();
}

// ── 配置加载与渲染 ────────────────────────────────────────────────────────────

let currentConfig = { enabled: false, exposed_capabilities: [] };

async function loadConfig() {
  try {
    currentConfig = await invoke("get_mcp_server_config");
    await renderCapabilities();
    updateToggle();
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

/**
 * 根据开关状态显示/隐藏能力列表和使用方式。
 */
function updateDetailVisibility() {
  const detail = document.getElementById("mcp-server-detail");
  if (detail) {
    detail.classList.toggle('hidden', !currentConfig.enabled);
  }
}

// ── 使用方式：配置 JSON + 复制 ────────────────────────────────────────────────

/**
 * 生成 MCP 客户端配置 JSON 并填充到 <pre> 中。
 */
function updateConfigJson() {
  const pre = document.getElementById("mcp-server-config-json");
  if (!pre) return;

  const config = {
    mcpServers: {
      blink: {
        command: "blink",
        args: ["mcp-server"],
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
