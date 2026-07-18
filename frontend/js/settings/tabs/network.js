/**
 * 网络设置 Tab 模块
 * 包含：HTTP/HTTPS 代理配置
 */

import { invoke } from "../../tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { saveConfig } from "../../config-keys.js";
import { clearUnsaved, markUnsaved } from "../shared/ui.js";

/**
 * 初始化网络设置 Tab
 * @param {Object} cfg - 初始配置
 */
export function initNetworkTab(cfg) {
  loadNetworkConfig();
}

/**
 * 加载网络配置
 */
async function loadNetworkConfig() {
  const container = document.getElementById("network-container");
  if (!container) return;

  let proxyConfig = { http: "", https: "" };
  try {
    const cfg = await invoke("get_engine_config", { engineId: "_global_proxy" });
    if (cfg) {
      proxyConfig = { http: cfg.http || "", https: cfg.https || "" };
    }
  } catch (e) {
    console.error("load proxy config failed:", e);
  }

  container.innerHTML = renderNetworkCard(proxyConfig);
  bindNetworkEvents(container);

  // 语言切换时重新渲染（保留未保存的输入值，避免用户正在编辑时丢失）
  onLangChange(() => {
    const el = document.getElementById("network-container");
    if (!el) return;
    const httpInput = el.querySelector('.plugin-field[data-key="http_proxy"]');
    const httpsInput = el.querySelector('.plugin-field[data-key="https_proxy"]');
    const http = httpInput?.value ?? proxyConfig.http;
    const https = httpsInput?.value ?? proxyConfig.https;
    const renderCfg = { http, https };
    el.innerHTML = renderNetworkCard(renderCfg);
    bindNetworkEvents(el);
    // 如果输入值与已保存值不同，标记 unsaved 徽章
    if (http !== proxyConfig.http || https !== proxyConfig.https) {
      markUnsaved(el);
    }
  });
}

/**
 * 渲染网络配置卡片
 * @param {Object} proxyConfig - 代理配置
 * @returns {string} HTML 字符串
 */
function renderNetworkCard(proxyConfig) {
  return `
    <div class="extension-card">
      <div class="extension-header">
        <div class="extension-icon">🌐</div>
        <div class="extension-info">
          <h3>${t("network.title")}</h3>
          <p class="extension-desc">${t("network.desc")}</p>
        </div>
      </div>
      <div class="extension-body">
        <div class="setting-row">
          <label class="setting-label">${t("network.http.label")}</label>
          <input
            type="text"
            class="input-wide plugin-field"
            data-key="http_proxy"
            placeholder="${t("network.http.ph")}"
            value="${escapeAttr(proxyConfig.http)}"
          />
        </div>
        <div class="setting-row">
          <label class="setting-label">${t("network.https.label")}</label>
          <input
            type="text"
            class="input-wide plugin-field"
            data-key="https_proxy"
            placeholder="${t("network.https.ph")}"
            value="${escapeAttr(proxyConfig.https)}"
          />
        </div>
        <div class="setting-row">
          <button class="btn-primary plugin-save">${t("network.save")}</button>
          <span class="plugin-save-msg"></span>
        </div>
      </div>
    </div>
  `;
}

/**
 * 绑定网络事件
 * @param {HTMLElement} container - 容器元素
 */
function bindNetworkEvents(container) {
  const btn = container.querySelector(".plugin-save");
  const msg = container.querySelector(".plugin-save-msg");
  if (!btn) return;

  // 字段变更 → 挂 unsaved 徽章
  container.querySelectorAll('.plugin-field').forEach((el) => {
    el.addEventListener("input", () => markUnsaved(container));
    el.addEventListener("change", () => markUnsaved(container));
  });

  btn.addEventListener("click", async () => {
    const http = container.querySelector('.plugin-field[data-key="http_proxy"]')?.value || "";
    const https = container.querySelector('.plugin-field[data-key="https_proxy"]')?.value || "";

    try {
      await saveConfig("global_proxy", { http, https });
      if (msg) {
        msg.textContent = t("network.saved_msg");
        msg.className = "plugin-save-msg msg-success";
        setTimeout(() => {
          if (msg) {
            msg.textContent = "";
            msg.className = "plugin-save-msg";
          }
        }, 3000);
      }
      clearUnsaved(container);
    } catch (e) {
      console.error("save proxy failed:", e);
      if (msg) {
        msg.textContent = t("network.save_failed");
        msg.className = "plugin-save-msg msg-error";
      }
    }
  });
}

/**
 * HTML 转义
 * @param {string} str - 原始字符串
 * @returns {string} 转义后的字符串
 */
function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str;
  return div.innerHTML;
}

/**
 * 属性转义
 * @param {string} str - 原始字符串
 * @returns {string} 转义后的字符串
 */
function escapeAttr(str) {
  return str.replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
