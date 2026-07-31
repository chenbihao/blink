/**
 * 调试设置 Tab 模块
 * 包含：日志级别、日志文件、性能统计
 *
 * perf 部分搬自原 settings.js loadPerfStats/renderPerfStats 等（0.9.5 拆分时被残缺重写，0.9.5.1 还原）。
 */
import { invoke, confirmDialog } from "../../shared/tauri.js";
import { saveConfig } from "../../shared/config-keys.js";
import { t, onLangChange } from "../../i18n/index.js";
import { getCurrentConfig } from "../shared/state.js";

/** 缓存性能统计概览，语言切换时用它重渲染（避免重新 IPC 探测） */
let _cachedOverview = null;
/** 防止重复注册 onLangChange */
let _langChangeRegistered = false;

/**
 * 初始化调试设置 Tab
 */
export function initDebugTab() {
  initLogSettings();
  initPerfStats();

  // 语言切换时用缓存数据重渲染性能统计（no_data / unit.ms / stats 等带参数文案）
  if (!_langChangeRegistered) {
    _langChangeRegistered = true;
    onLangChange(() => {
      if (_cachedOverview) renderPerfStats(_cachedOverview);
    });
  }
}

// ── 日志 ────────────────────────────────────────────────────────────────────

/**
 * 初始化日志设置
 */
function initLogSettings() {
  const logLevelSelect = document.getElementById("log-level");
  if (logLevelSelect) {
    logLevelSelect.addEventListener("change", async (e) => {
      try {
        await saveConfig("log_level", e.target.value);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.log_level = e.target.value;
      } catch (err) {
        console.error("update_log_level failed:", err);
      }
    });
  }

  // AI 详细日志开关（0.12.6）
  const aiVerboseCheckbox = document.getElementById("ai-verbose-log");
  if (aiVerboseCheckbox) {
    aiVerboseCheckbox.addEventListener("change", async (e) => {
      try {
        await saveConfig("ai_verbose_log", e.target.checked);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.ai_verbose_log = e.target.checked;
      } catch (err) {
        console.error("update_ai_verbose_log failed:", err);
      }
    });
  }

  document.getElementById("open-log-file")?.addEventListener("click", async () => {
    try {
      await invoke("open_log_file");
    } catch (e) {
      console.error("open_log_file failed:", e);
    }
  });

  document.getElementById("open-log-dir")?.addEventListener("click", async () => {
    try {
      await invoke("open_log_dir");
    } catch (e) {
      console.error("open_log_dir failed:", e);
    }
  });

  loadLogInfo();
  loadAiVerboseState();
}

/**
 * 加载 AI 详细日志开关初始状态
 */
async function loadAiVerboseState() {
  try {
    const config = await invoke("get_config");
    const checkbox = document.getElementById("ai-verbose-log");
    if (checkbox && config?.ai_verbose_log != null) {
      checkbox.checked = config.ai_verbose_log;
    }
  } catch (e) {
    console.error("loadAiVerboseState failed:", e);
  }
}

/**
 * 加载日志文件信息
 */
async function loadLogInfo() {
  try {
    const info = await invoke("get_log_info");
    const el = document.getElementById("log-file-path");
    if (el) el.textContent = info.current_file || "-";
  } catch (e) {
    console.error("loadLogInfo failed:", e);
  }
}

// ── 性能统计（搬自原 settings.js）────────────────────────────────────────────

/**
 * 初始化性能统计
 */
function initPerfStats() {
  loadPerfStats();

  document.getElementById("perf-refresh")?.addEventListener("click", () => loadPerfStats());
  document.getElementById("perf-export")?.addEventListener("click", exportPerfReport);
  document.getElementById("perf-clear")?.addEventListener("click", clearPerfStats);
}

/**
 * 加载性能统计
 */
async function loadPerfStats() {
  try {
    const overview = await invoke("get_perf_overview");
    _cachedOverview = overview;
    renderPerfStats(overview);
  } catch (e) {
    console.error("loadPerfStats failed:", e);
    showPerfError();
  }
}

/**
 * 渲染性能统计总览
 * @param {Object} overview - get_perf_overview 返回
 */
function renderPerfStats(overview) {
  renderPercentileCard("perf-startup-total", overview.startup);
  renderPercentileCard("perf-hotkey-show", overview.hotkey);
  renderPercentileCard("perf-search-total", overview.search);

  // 采样数：取三项中最大的 count
  const countEl = document.getElementById("perf-total-count");
  if (countEl) {
    const counts = [overview.startup, overview.hotkey, overview.search]
      .filter((d) => d && d.count > 0)
      .map((d) => d.count);
    countEl.textContent = counts.length > 0 ? Math.max(...counts) : t("debug.perf.no_data");
    countEl.className = counts.length > 0 ? "debug-value" : "debug-value no-data";
  }

  // 慢查询日志（合并 hotkey + search）
  renderSlowQueries([...(overview.slow_hotkey || []), ...(overview.slow_search || [])]);
}

/**
 * 渲染单个百分位数卡片
 * @param {string} elementId - 容器 id
 * @param {Object} data - 采样数据 { count, p50, p90, p99, min, max, avg }
 */
function renderPercentileCard(elementId, data) {
  const el = document.getElementById(elementId);
  if (!el) return;

  if (!data || data.count === 0) {
    el.textContent = t("debug.perf.no_data");
    el.className = "debug-value no-data";
    return;
  }

  // 显示 P50 作为主值，P90/P99 作为 title 提示
  const p50 = data.p50 || "-";
  el.textContent = `${p50} ${t("debug.perf.unit.ms")}`;
  el.className = "debug-value";
  el.title = [
    `${t("debug.perf.stats.count")}: ${data.count}`,
    `${t("debug.perf.stats.p50")}: ${data.p50} ms`,
    `${t("debug.perf.stats.p90")}: ${data.p90} ms`,
    `${t("debug.perf.stats.p99")}: ${data.p99} ms`,
    `${t("debug.perf.stats.min")}: ${data.min} ms`,
    `${t("debug.perf.stats.max")}: ${data.max} ms`,
    `${t("debug.perf.stats.avg")}: ${data.avg} ms`,
  ].join("\n");
}

/**
 * 渲染慢查询日志
 * @param {Array} slowItems - 慢查询列表
 */
function renderSlowQueries(slowItems) {
  const el = document.getElementById("perf-slow-list");
  if (!el) return;

  if (!slowItems || slowItems.length === 0) {
    el.innerHTML = `<div class="perf-slow-empty">${t("debug.perf.slow.empty")}</div>`;
    return;
  }

  // 按耗时降序
  slowItems.sort((a, b) => b.value_ms - a.value_ms);

  el.innerHTML = slowItems.map((m) => `
    <div class="perf-slow-item">
      <span class="perf-slow-cat">${escapeHtml(m.category)}</span>
      <span class="perf-slow-name">${escapeHtml(m.name)}</span>
      <span class="perf-slow-time">${m.value_ms.toFixed(1)} ms</span>
      <span class="perf-slow-meta">${escapeHtml(m.metadata || "")}</span>
    </div>
  `).join("");
}

/**
 * 显示性能统计错误状态
 */
function showPerfError() {
  ["perf-startup-total", "perf-hotkey-show", "perf-search-total", "perf-total-count"].forEach((id) => {
    const el = document.getElementById(id);
    if (el) {
      el.textContent = "-";
      el.className = "debug-value error";
    }
  });
}

/**
 * 导出性能报告
 */
async function exportPerfReport() {
  try {
    const path = await invoke("export_perf_report");
    if (!path) return; // 用户取消了
    const btn = document.getElementById("perf-export");
    if (btn) {
      const original = btn.textContent;
      btn.textContent = t("debug.perf.exported");
      setTimeout(() => { btn.textContent = original; }, 2000);
    }
    console.log("性能报告已保存到:", path);
  } catch (e) {
    console.error("export_perf_report failed:", e);
  }
}

/**
 * 清除性能采样记录
 */
async function clearPerfStats() {
  const ok = await confirmDialog(t("debug.perf.clear.confirm"), {
    title: t("common.confirm"),
    kind: "warning",
  });
  if (!ok) return;
  try {
    await invoke("clear_perf_data");
    loadPerfStats();
    const btn = document.getElementById("perf-clear");
    if (btn) {
      const original = btn.textContent;
      btn.textContent = t("debug.perf.cleared");
      setTimeout(() => { btn.textContent = original; }, 2000);
    }
  } catch (e) {
    console.error("clear_perf_data failed:", e);
  }
}

/** HTML 转义 */
function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str;
  return div.innerHTML;
}
