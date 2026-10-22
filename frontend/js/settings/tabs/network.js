/**
 * 网络设置 Tab 模块
 * 包含：HTTP/HTTPS 代理配置（0.27.4：迁移到 network 配置分片，保存即热更新）
 */

import {invoke, normalizeError} from "../../shared/tauri.js";
import {onLangChange, t} from "../../i18n/index.js";
import {iconHTML} from "../../shared/icon.js";
import {saveConfig} from "../../shared/config-keys.js";

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

    let proxyConfig = {http: "", https: ""};
    try {
        const cfg = await invoke("get_config_section", {key: "network"});
        if (cfg) {
            proxyConfig = {http: cfg.http || "", https: cfg.https || ""};
        }
    } catch (e) {
        console.error("load proxy config failed:", e);
    }

    container.innerHTML = renderNetworkCard(proxyConfig);
    bindNetworkEvents(container);

    // 语言切换时重新渲染（自动保存模式，直接用已保存值重渲染）
    onLangChange(() => {
        const el = document.getElementById("network-container");
        if (!el) return;
        el.innerHTML = renderNetworkCard(proxyConfig);
        bindNetworkEvents(el);
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
        <div class="extension-icon">${iconHTML("globe")}</div>
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
        <p class="network-status hidden" id="network-save-status"></p>
        <p class="extension-desc">${t("network.note.direct")}</p>
        <p class="extension-desc">${t("network.note.boundary")}</p>
      </div>
    </div>
  `;
}

/**
 * 绑定网络事件（自动保存：输入变更 → debounce 保存）
 *
 * 0.27.4：保存走 `network` 分片——后端校验通过后落库并热更新 net 工厂
 * （AI/更新/下载/MCP 立即生效，无需重启）；无效地址保留旧配置并回显错误。
 * @param {HTMLElement} container - 容器元素
 */
function bindNetworkEvents(container) {
    let debounceTimer = null;

    const showStatus = (message, isError) => {
        const el = container.querySelector("#network-save-status");
        if (!el) return;
        if (!message) {
            el.classList.add("hidden");
            el.textContent = "";
            return;
        }
        el.classList.remove("hidden");
        el.textContent = message;
        el.classList.toggle("network-status-error", isError === true);
    };

    const doSave = async () => {
        const http = container.querySelector('.plugin-field[data-key="http_proxy"]')?.value || "";
        const https = container.querySelector('.plugin-field[data-key="https_proxy"]')?.value || "";
        try {
            await saveConfig("network", {http, https});
            showStatus(t("network.saved"), false);
        } catch (e) {
            console.error("save proxy failed:", e);
            const err = normalizeError(e);
            showStatus(
                err.code === "network_invalid" ? t("network.err.invalid") : err.message,
                true,
            );
        }
    };

    // 文本输入 debounce 800ms（避免每 keystroke 都写盘）
    container.querySelectorAll(".plugin-field").forEach((el) => {
        el.addEventListener("input", () => {
            showStatus("", false);
            clearTimeout(debounceTimer);
            debounceTimer = setTimeout(doSave, 800);
        });
    });
}

/**
 * 属性转义
 * @param {string} str - 原始字符串
 * @returns {string} 转义后的字符串
 */
function escapeAttr(str) {
    return str.replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
