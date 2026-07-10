/**
 * 搜索引擎 Tab 模块
 * 包含：应用搜索、文件搜索、计算器配置
 *
 * 保存策略：全部即时保存（change 事件 → saveConfig），不保留"保存配置"按钮。
 * 与其它自动保存卡片（context / general / chord）一致。
 */

import { invoke, messageDialog } from "../../tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { saveConfig } from "../../config-keys.js";

/**
 * 初始化搜索引擎 Tab
 * @param {Object} cfg - 初始配置
 */
export function initEnginesTab(cfg) {
  // 回填配置（0.9.5 拆分时丢失的 loadEngineConfig，0.9.5.1 补回）
  loadEngineConfig();
  initStartMenuConfig();
  initCalcConfig();
  initFileSearchConfig();
  initInterpreterProbing();
  // 状态徽章文本是 JS 动态生成（探测结果 → i18n key），applyI18n 扫不到，
  // 语言切换时通过 i18n 订阅自行刷新
  onLangChange(() => {
    refreshEverythingBadgeText();
    refreshInterpreterBadgeText("python");
    refreshInterpreterBadgeText("node");
  });
}

/**
 * 加载并回填搜索引擎配置（应用搜索 / 文件搜索 / 计算器）
 * 拆自原 settings.js loadEngineConfig；loadBuiltinActions/loadPlugins 已归 plugins.js，不在此调。
 */
async function loadEngineConfig() {
  // 应用搜索（开始菜单）
  try {
    const startMenu = await invoke("get_start_menu_config");
    const enabledEl = document.getElementById("start-menu-enabled");
    const depthEl = document.getElementById("start-menu-scan-depth");
    const includeUwpEl = document.getElementById("start-menu-include-uwp");
    if (enabledEl) enabledEl.checked = startMenu.enabled !== false;
    if (depthEl) depthEl.value = startMenu.scan_depth || 3;
    if (includeUwpEl) includeUwpEl.checked = startMenu.include_uwp !== false;
  } catch (e) {
    console.error("loadStartMenuConfig failed:", e);
  }

  // 文件搜索
  try {
    const fileSearch = await invoke("get_engine_config", { engineId: "file_search" });
    const enabled = fileSearch.enabled !== false;
    const dataSource = fileSearch.data_source || "auto";
    const port = fileSearch.everything_port || 80;
    const maxResults = fileSearch.max_results || 20;

    const enabledEl = document.getElementById("file-search-enabled");
    const dataSourceEl = document.getElementById("file-search-data-source");
    const portEl = document.getElementById("everything-port");
    const maxResultsEl = document.getElementById("everything-max-results");

    if (enabledEl) enabledEl.checked = enabled;
    if (dataSourceEl) dataSourceEl.value = dataSource;
    if (portEl) portEl.value = port;
    if (maxResultsEl) maxResultsEl.value = maxResults;

    // 页面加载后自动探测一次（非 local 模式）
    if (dataSource !== "local") {
      setTimeout(probeEverythingStatus, 500);
    }
  } catch (e) {
    console.error("loadFileSearchConfig failed:", e);
    const portEl = document.getElementById("everything-port");
    if (portEl) portEl.value = 80;
  }

  // 计算器
  try {
    const calc = await invoke("get_calc_config");
    const enabledEl = document.getElementById("calc-enabled");
    if (enabledEl) enabledEl.checked = calc.enabled !== false;
  } catch (e) {
    console.error("loadCalcConfig failed:", e);
  }
}

/**
 * 初始化应用搜索配置：三个字段（enabled / scan_depth / include_uwp）任一变更即保存
 */
function initStartMenuConfig() {
  const enabledEl = document.getElementById("start-menu-enabled");
  const depthEl = document.getElementById("start-menu-scan-depth");
  const includeUwpEl = document.getElementById("start-menu-include-uwp");

  const save = async () => {
    const enabled = enabledEl?.checked ?? true;
    const scanDepth = parseInt(depthEl?.value, 10) || 3;
    const includeUwp = includeUwpEl?.checked ?? true;
    try {
      await saveConfig("start_menu_config", { enabled, scan_depth: scanDepth, include_uwp: includeUwp });
    } catch (e) {
      console.error("update_start_menu_config failed:", e);
      messageDialog(t("common.save_failed_msg", { err: e }), { title: t("common.error"), kind: "error" });
    }
  };

  enabledEl?.addEventListener("change", save);
  depthEl?.addEventListener("change", save);
  includeUwpEl?.addEventListener("change", save);
}

/**
 * 初始化计算器配置
 */
function initCalcConfig() {
  document.getElementById("calc-enabled")?.addEventListener("change", async (e) => {
    try {
      await saveConfig("calc_config", { enabled: e.target.checked });
    } catch (err) {
      console.error("update_calc_config failed:", err);
      e.target.checked = !e.target.checked;
    }
  });
}

/**
 * 初始化文件搜索配置：任一字段变更即保存；探测按钮独立
 */
function initFileSearchConfig() {
  // 探测 Everything 状态
  document.getElementById("probe-everything")?.addEventListener("click", probeEverythingStatus);

  const enabledEl = document.getElementById("file-search-enabled");
  const dataSourceEl = document.getElementById("file-search-data-source");
  const portEl = document.getElementById("everything-port");
  const maxResultsEl = document.getElementById("everything-max-results");

  [enabledEl, dataSourceEl, portEl, maxResultsEl].forEach((el) => {
    el?.addEventListener("change", saveFileSearchConfig);
  });
}

/**
 * 保存文件搜索配置（校验端口 + saveConfig + 视需重探）
 */
async function saveFileSearchConfig() {
  const enabled = document.getElementById("file-search-enabled")?.checked ?? true;
  const dataSource = document.getElementById("file-search-data-source")?.value || "auto";
  const port = parseInt(document.getElementById("everything-port")?.value, 10);
  const maxResults = parseInt(document.getElementById("everything-max-results")?.value, 10) || 20;

  if (!Number.isFinite(port) || port < 1 || port > 65535) {
    // 用户还在输入中（例如清空端口再输），静默跳过；等输到合法值再自动保存
    console.warn("[engines] file-search port invalid, skip auto-save:", port);
    return;
  }

  try {
    await saveConfig("file_search", {
      enabled,
      data_source: dataSource,
      everything_port: port,
      max_results: maxResults,
    });
    if (dataSource !== "local") {
      probeEverythingStatus();
    }
  } catch (e) {
    console.error("update_file_search failed:", e);
    messageDialog(t("common.save_failed_msg", { err: e }), { title: t("common.error"), kind: "error" });
  }
}

/**
 * 探测 Everything 状态
 */
async function probeEverythingStatus() {
  const statusEl = document.getElementById("everything-status");
  const portInput = document.getElementById("everything-port");
  const port = parseInt(portInput?.value || "80", 10);

  statusEl.textContent = t("engine.status.probing");
  statusEl.className = "status-badge status-unknown";
  statusEl.dataset.badgeState = "probing";

  try {
    const available = await invoke("probe_everything", { port });
    if (available) {
      statusEl.textContent = t("engine.status.available");
      statusEl.className = "status-badge status-available";
      statusEl.dataset.badgeState = "available";
    } else {
      statusEl.textContent = t("engine.status.unavailable");
      statusEl.className = "status-badge status-unavailable";
      statusEl.dataset.badgeState = "unavailable";
    }
  } catch (e) {
    statusEl.textContent = t("engine.status.failed");
    statusEl.className = "status-badge status-unavailable";
    statusEl.dataset.badgeState = "failed";
    console.error("probe_everything failed:", e);
  }
}

/**
 * 刷新 Everything 徽章文本（语言切换时）
 */
export function refreshEverythingBadgeText() {
  const statusEl = document.getElementById("everything-status");
  if (!statusEl) return;
  const key =
    statusEl.dataset.badgeState === "available" ? "engine.status.available" :
    statusEl.dataset.badgeState === "unavailable" ? "engine.status.unavailable" :
    statusEl.dataset.badgeState === "failed" ? "engine.status.failed" :
    "engine.status.probing";
  statusEl.textContent = t(key);
}

/**
 * 刷新脚本解释器徽章文本（语言切换时）
 * @param {string} type - 解释器类型（python/node）
 */
export function refreshInterpreterBadgeText(type) {
  const statusEl = document.getElementById(`${type}-status`);
  if (!statusEl) return;
  const key =
    statusEl.dataset.badgeState === "available" ? "engine.status.available" :
    statusEl.dataset.badgeState === "version_low" ? "engine.status.version_low" :
    statusEl.dataset.badgeState === "not_found" ? "engine.status.not_found" :
    statusEl.dataset.badgeState === "failed" ? "engine.status.failed" :
    "engine.status.probing";
  statusEl.textContent = t(key);
}

// ── 脚本解释器探测（搬自原 settings.js，0.9.5 拆分时遗漏，0.9.5.1 补回）─────────

/**
 * 更新单个解释器的状态 UI
 * @param {"python"|"node"} type - 解释器类型
 * @param {Object} status - 后端探测结果 { found, version_ok, version, path, error }
 */
function updateInterpreterUI(type, status) {
  const statusEl = document.getElementById(`${type}-status`);
  const pathEl = document.getElementById(`${type}-path`);
  if (!statusEl) return;

  if (status.found) {
    if (status.version_ok) {
      const versionText = status.version ? `${status.version} ` : "";
      statusEl.textContent = `${versionText}${t("engine.status.available")}`;
      statusEl.className = "status-badge status-available";
      statusEl.dataset.badgeState = "available";
    } else {
      const versionText = status.version ? `${status.version} ` : "";
      statusEl.textContent = `${versionText}${t("engine.status.version_low")}`;
      statusEl.className = "status-badge status-warning";
      statusEl.dataset.badgeState = "version_low";
    }
    if (pathEl) pathEl.value = status.path || "";
  } else {
    statusEl.textContent = t("engine.status.not_found");
    statusEl.className = "status-badge status-unavailable";
    statusEl.dataset.badgeState = "not_found";
    if (pathEl) pathEl.value = status.error || t("engine.status.not_found");
  }
}

/**
 * 打开文件选择器选择解释器路径
 * @param {"python"|"node"} kind - 解释器类型
 */
async function browseInterpreter(kind) {
  try {
    const selected = await invoke("open_file_dialog", {
      title: t(`file_dialog.${kind}_title`),
      filters: [{ name: t("file_dialog.exe_filter"), extensions: ["exe"] }],
    });
    if (selected) {
      const pathEl = document.getElementById(`${kind}-path`);
      if (pathEl) pathEl.value = selected;
    }
  } catch (e) {
    console.error("browseInterpreter failed:", e);
  }
}

/**
 * 探测单个解释器（一次只探测一种，跟文件搜索对齐）
 * @param {"python"|"node"} type - 解释器类型
 */
async function probeSingleInterpreter(type) {
  const statusEl = document.getElementById(`${type}-status`);
  if (!statusEl) return;

  statusEl.textContent = t("engine.status.probing");
  statusEl.className = "status-badge status-unknown";
  statusEl.dataset.badgeState = "probing";

  try {
    const status = await invoke("probe_interpreters");
    updateInterpreterUI(type, status[type]);
  } catch (e) {
    console.error(`probeInterpreter ${type} failed:`, e);
    statusEl.textContent = t("engine.status.failed");
    statusEl.className = "status-badge status-unavailable";
    statusEl.dataset.badgeState = "failed";
  }
}

/**
 * 探测全部解释器（只在首次启动两个路径都为空时自动调用，避免覆盖用户手动配置）
 */
async function probeAllInterpreters() {
  const pythonPath = document.getElementById("python-path")?.value;
  const nodePath = document.getElementById("node-path")?.value;
  if (pythonPath && nodePath) return;

  ["python", "node"].forEach((type) => {
    const statusEl = document.getElementById(`${type}-status`);
    if (statusEl) {
      statusEl.textContent = t("engine.status.probing");
      statusEl.className = "status-badge status-unknown";
      statusEl.dataset.badgeState = "probing";
    }
  });

  try {
    const status = await invoke("probe_interpreters");
    updateInterpreterUI("python", status.python);
    updateInterpreterUI("node", status.node);
  } catch (e) {
    console.error("probeInterpreters failed:", e);
  }
}

/**
 * 初始化脚本解释器探测：绑定按钮事件 + 首次自动探测
 */
function initInterpreterProbing() {
  document.getElementById("python-probe")?.addEventListener("click", () => probeSingleInterpreter("python"));
  document.getElementById("node-probe")?.addEventListener("click", () => probeSingleInterpreter("node"));
  document.getElementById("python-browse")?.addEventListener("click", () => browseInterpreter("python"));
  document.getElementById("node-browse")?.addEventListener("click", () => browseInterpreter("node"));

  // 首次启动：两个路径都为空时才自动探测
  setTimeout(() => {
    const pythonPath = document.getElementById("python-path")?.value;
    const nodePath = document.getElementById("node-path")?.value;
    if (!pythonPath && !nodePath) {
      probeAllInterpreters();
    }
  }, 100);
}
