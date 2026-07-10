/**
 * 存储设置 Tab 模块
 * 包含：历史记录、数据库路径、清理操作
 */

import { invoke } from "../../tauri.js";
import { confirmDialog } from "../../tauri.js";
import { t } from "../../i18n/index.js";

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
async function loadStorageInfo() {
  try {
    const info = await invoke("get_storage_info");
    document.getElementById("history-count").textContent = t("storage.history_count", { count: info.history_count });
    document.getElementById("db-path").textContent = info.db_path;
  } catch (e) {
    console.error("loadStorageInfo failed:", e);
  }
}
