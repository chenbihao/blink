/**
 * 存储设置 Tab 模块
 * 包含：历史记录、数据库路径、清理操作
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

function renderStorageInfo() {
  if (!_cachedInfo) return;
  const histEl = document.getElementById("history-count");
  const dbEl = document.getElementById("db-path");
  if (histEl) histEl.textContent = t("storage.history_count", { count: _cachedInfo.history_count });
  if (dbEl) dbEl.textContent = _cachedInfo.db_path;
}

// 语言切换时重新渲染文本（history_count 带参数，data-i18n 无法处理）
onLangChange(renderStorageInfo);
