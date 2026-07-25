/**
 * 存储设置 Tab 模块
 * 包含：四库统计（config / history / ai / cache）+ 清理操作 + 打开文件夹
 */

import { invoke } from "../../tauri.js";
import { confirmDialog } from "../../tauri.js";
import { t, onLangChange } from "../../i18n/index.js";

/**
 * 初始化存储设置 Tab
 */
export function initStorageTab() {
  loadStorageInfo();

  document.getElementById("clear-history")?.addEventListener("click", async () => {
    const ok = await confirmDialog(t("storage.clear.confirm"), {
      title: t("common.confirm"),
      kind: "warning",
    });
    if (!ok) return;
    await invoke("clear_history");
    loadStorageInfo();
  });

  document.getElementById("clear-ai-audit")?.addEventListener("click", async () => {
    const ok = await confirmDialog(t("storage.clear_audit.confirm"), {
      title: t("common.confirm"),
      kind: "warning",
    });
    if (!ok) return;
    await invoke("clear_ai_audit");
    loadStorageInfo();
  });

  document.getElementById("clear-cache-db")?.addEventListener("click", async () => {
    const ok = await confirmDialog(t("storage.clear_cache.confirm"), {
      title: t("common.confirm"),
      kind: "warning",
    });
    if (!ok) return;
    try {
      await invoke("clear_cache_db");
      loadStorageInfo();
    } catch (e) {
      console.error("clear_cache_db failed:", e);
    }
  });

  document.getElementById("open-data-folder")?.addEventListener("click", async () => {
    try {
      await invoke("open_data_folder");
    } catch (e) {
      console.error("open_data_folder failed:", e);
    }
  });

  document.getElementById("retry-migration")?.addEventListener("click", async () => {
    try {
      await invoke("retry_migration");
      // 清除标记后隐藏警告条，提示用户重启
      const warnEl = document.getElementById("migration-warning");
      if (warnEl) warnEl.hidden = true;
      alert(t("storage.retry_migration.done"));
    } catch (e) {
      console.error("retry_migration failed:", e);
    }
  });
}

/**
 * 加载存储信息
 */
let _cachedInfo = null;

async function loadStorageInfo() {
  try {
    _cachedInfo = await invoke("get_storage_info");
    renderStorageInfo();
  } catch (e) {
    console.error("loadStorageInfo failed:", e);
  }
}

/**
 * 格式化文件大小
 */
function formatSize(bytes) {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
  return (bytes / (1024 * 1024)).toFixed(1) + " MB";
}

function renderStorageInfo() {
  if (!_cachedInfo) return;

  // P2.7: 迁移失败警告（旧 blink.db 迁移失败时显示）
  const warnEl = document.getElementById("migration-warning");
  if (warnEl) {
    if (_cachedInfo.migration_failed) {
      warnEl.hidden = false;
      warnEl.title = _cachedInfo.migration_failed;
    } else {
      warnEl.hidden = true;
    }
  }

  // 数据目录
  const dirEl = document.getElementById("data-dir");
  if (dirEl) dirEl.textContent = _cachedInfo.data_dir || "-";

  const dbs = _cachedInfo.databases || {};

  // 配置库
  setText("db-config-size", dbs.config ? formatSize(dbs.config.size_bytes) : "-");
  setText("db-config-path", dbs.config?.path || "-");

  // 历史库
  setText("db-history-size", dbs.history ? formatSize(dbs.history.size_bytes) : "-");
  setText("db-history-path", dbs.history?.path || "-");
  setText(
    "db-history-count",
    dbs.history ? t("storage.stat.history", { count: dbs.history.history_count ?? 0 }) : "-"
  );
  setText(
    "db-clipboard-count",
    dbs.history ? t("storage.stat.clipboard", { count: dbs.history.clipboard_count ?? 0 }) : "-"
  );

  // AI 库
  setText("db-ai-size", dbs.ai ? formatSize(dbs.ai.size_bytes) : "-");
  setText("db-ai-path", dbs.ai?.path || "-");
  setText(
    "db-audit-count",
    dbs.ai ? t("storage.stat.audit", { count: dbs.ai.audit_count ?? 0 }) : "-"
  );

  // 缓存库
  setText("db-cache-size", dbs.cache ? formatSize(dbs.cache.size_bytes) : "-");
  setText("db-cache-path", dbs.cache?.path || "-");
  setText(
    "db-perf-count",
    dbs.cache ? t("storage.stat.perf", { count: dbs.cache.perf_count ?? 0 }) : "-"
  );
  setText(
    "db-icon-cache-count",
    dbs.cache ? t("storage.stat.icon_cache", { count: dbs.cache.icon_cache_count ?? 0 }) : "-"
  );
}

function setText(id, text) {
  const el = document.getElementById(id);
  if (el) el.textContent = text;
}

// 语言切换时重新渲染文本（带参数的 i18n 需要手动重渲染）
onLangChange(renderStorageInfo);
