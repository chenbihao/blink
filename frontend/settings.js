const TAU = window.__TAURI__;
const invoke = TAU?.core?.invoke ?? TAU?.invoke;

// ── Tab 切换 ─────────────────────────────────────────────────────────────────

document.querySelectorAll(".tab").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
    document.querySelectorAll(".panel").forEach((p) => p.classList.remove("active"));
    btn.classList.add("active");
    document.getElementById(btn.dataset.tab).classList.add("active");
  });
});

// ── 调试面板（Phase 4）───────────────────────────────────────────────────────

const debugLog = document.getElementById("debug-log");
let logLines = [];

TAU?.event?.listen("blink://debug", (e) => {
  const d = e.payload || {};
  document.getElementById("debug-invoke").textContent = d.invoke_ms != null ? `${d.invoke_ms}ms` : "-";
  document.getElementById("debug-show").textContent = d.show_ms != null ? `${d.show_ms}ms` : "-";
  document.getElementById("debug-focus").textContent = d.focus_ms != null ? `${d.focus_ms}ms` : "-";
  document.getElementById("debug-rate").textContent = d.success_rate ?? "-";

  // 日志追加（最多保留 200 行）
  const ts = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  logLines.push(`[${ts}] invoke=${d.invoke_ms ?? "-"}ms show=${d.show_ms ?? "-"}ms focus=${d.focus_ms ?? "-"}ms`);
  if (logLines.length > 200) logLines.shift();
  debugLog.value = logLines.join("\n");
  debugLog.scrollTop = debugLog.scrollHeight;
});

document.getElementById("clear-log")?.addEventListener("click", () => {
  logLines = [];
  debugLog.value = "";
});

// ── 存储面板 ─────────────────────────────────────────────────────────────────

async function loadStorageInfo() {
  try {
    const info = await invoke("get_storage_info");
    document.getElementById("history-count").textContent = `${info.history_count} 条记录`;
    document.getElementById("db-path").textContent = info.db_path;
  } catch (e) {
    console.error("loadStorageInfo failed:", e);
  }
}

document.getElementById("clear-history")?.addEventListener("click", async () => {
  if (confirm("确定清空所有历史记录？")) {
    await invoke("clear_history");
    loadStorageInfo();
  }
});

// 初始化
loadStorageInfo();

// ── 快捷键滑块 ───────────────────────────────────────────────────────────────

document.getElementById("tap-threshold")?.addEventListener("input", (e) => {
  document.getElementById("tap-threshold-value").textContent = `${e.target.value}ms`;
});

document.getElementById("grace-ms")?.addEventListener("input", (e) => {
  document.getElementById("grace-ms-value").textContent = `${e.target.value}ms`;
});
