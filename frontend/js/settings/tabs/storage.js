/**
 * 存储设置 Tab 模块
 * 包含：四库统计（config / history / ai / cache）+ 清理操作 + 打开文件夹
 */

import {confirmDialog, invoke, messageDialog} from "../../shared/tauri.js";
import {onLangChange, t} from "../../i18n/index.js";
import {iconHTML} from "../../shared/icon.js";
import {clearClipboardImages, optimizeStorage} from "../../shared/api.js";
import {saveConfig} from "../../shared/config-keys.js";

/**
 * 初始化存储设置 Tab
 */
export function initStorageTab() {
    loadStorageInfo();
    loadCleanupInfo();

    document.getElementById("clear-history")?.addEventListener("click", async () => {
        const ok = await confirmDialog(t("storage.clear.confirm"), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        await invoke("clear_history");
        loadStorageInfo();
    });

    document.getElementById("clear-clipboard")?.addEventListener("click", async () => {
        const ok = await confirmDialog(t("storage.clear_clipboard.confirm"), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        await invoke("clear_clipboard_history");
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

    document.getElementById("clear-all-conversations")?.addEventListener("click", async () => {
        const ok = await confirmDialog(t("storage.clear_conversations.confirm"), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        await invoke("clear_all_conversations");
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

    document.getElementById("clear-clipboard-images")?.addEventListener("click", async () => {
        const ok = await confirmDialog(t("storage.clear_clipboard_images.confirm"), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        try {
            await clearClipboardImages();
            loadStorageInfo();
        } catch (e) {
            console.error("clear_clipboard_images failed:", e);
        }
    });

    document.getElementById("optimize-storage")?.addEventListener("click", async () => {
        const ok = await confirmDialog(t("storage.optimize.confirm"), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        const btn = document.getElementById("optimize-storage");
        if (btn) btn.disabled = true;
        try {
            const res = await optimizeStorage();
            await loadStorageInfo();
            const failures = (res.results || []).filter((r) => !r.success);
            if (failures.length > 0) {
                await messageDialog(
                    t("storage.optimize.partial_failed", {count: failures.length}),
                    {kind: "warning"}
                );
            } else {
                await messageDialog(t("storage.optimize.success"), {kind: "info"});
            }
        } catch (e) {
            console.error("optimize_storage failed:", e);
            await messageDialog(t("storage.optimize.failed", {err: String(e)}), {kind: "error"});
        } finally {
            if (btn) btn.disabled = false;
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
            console.info("[storage] retry_migration 成功");
            // 刷新存储信息（get_storage_info 会再次检查并清除残留标记）
            await loadStorageInfo();
        } catch (e) {
            console.error("retry_migration failed:", e);
            await messageDialog(t("storage.retry_migration.failed", {err: String(e)}), {kind: "error"});
        }
    });

    document.getElementById("reset-first-run")?.addEventListener("click", async () => {
        const ok = await confirmDialog(t("storage.reset_first_run.confirm"), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        try {
            await invoke("set_config", {key: "first_run", value: true});
            await messageDialog(t("storage.reset_first_run.done"), {kind: "info"});
        } catch (e) {
            console.error("reset_first_run failed:", e);
            await messageDialog(t("storage.reset_first_run.failed", {err: String(e)}), {kind: "error"});
        }
    });

    document.getElementById("clear-debug-flags")?.addEventListener("click", async () => {
        const ok = await confirmDialog(t("storage.clear_debug_flags.confirm"), {
            title: t("common.confirm"),
            kind: "warning",
        });
        if (!ok) return;
        try {
            // 重置 scroll_debug / ocr_debug 为默认值 false（prewarmOcr / controlSnap / windowEdgeSnap 保持默认）
            await saveConfig("screenshot_config", {
                prewarmOcr: true,
                scrollDebug: false,
                ocrDebug: false,
                controlSnap: true,
                windowEdgeSnap: 10
            });
            // 通知 chord tab 刷新截图开关状态（跨 tab 通知，避免 stale DOM）
            document.dispatchEvent(new CustomEvent("blink:config-changed", {detail: {key: "screenshot_config"}}));
            await messageDialog(t("storage.clear_debug_flags.done"), {kind: "info"});
        } catch (e) {
            console.error("clear_debug_flags failed:", e);
            await messageDialog(t("storage.clear_debug_flags.failed", {err: String(e)}), {kind: "error"});
        }
    });

    // 0.16.6: 一键清理全部数据（运行时禁用，仅卸载前手动启用）
    // 按钮已从 HTML 中移除，如需恢复请见 git history 的 cleanup-all-data 实现。
}

/**
 * 加载存储信息
 */
let _cachedInfo = null;

async function loadStorageInfo() {
    try {
        _cachedInfo = await invoke("get_storage_info");
        console.info("[storage] migration_failed:", _cachedInfo.migration_failed);
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

    // 历史库
    setText("db-history-size", dbs.history ? formatSize(dbs.history.size_bytes) : "-");
    setText(
        "db-history-count",
        dbs.history ? t("storage.stat.history", {count: dbs.history.history_count ?? 0}) : "-"
    );
    setText(
        "db-clipboard-count",
        dbs.history ? t("storage.stat.clipboard", {count: dbs.history.clipboard_count ?? 0}) : "-"
    );

    // AI 库
    setText("db-ai-size", dbs.ai ? formatSize(dbs.ai.size_bytes) : "-");
    setText(
        "db-audit-count",
        dbs.ai ? t("storage.stat.audit", {count: dbs.ai.audit_count ?? 0}) : "-"
    );
    setText(
        "db-conversation-count",
        dbs.ai ? t("storage.stat.conversations", {count: dbs.ai.conversation_count ?? 0}) : "-"
    );
    setText(
        "db-message-count",
        dbs.ai ? t("storage.stat.messages", {count: dbs.ai.message_count ?? 0}) : "-"
    );

    // 缓存库
    setText("db-cache-size", dbs.cache ? formatSize(dbs.cache.size_bytes) : "-");
    setText(
        "db-perf-count",
        dbs.cache ? t("storage.stat.perf", {count: dbs.cache.perf_count ?? 0}) : "-"
    );
    setText(
        "db-icon-cache-count",
        dbs.cache ? t("storage.stat.icon_cache", {count: dbs.cache.icon_cache_count ?? 0}) : "-"
    );
    // 0.17.0: 剪贴板图片统计（张数 + 占用空间合并展示）
    setText(
        "db-clipboard-image-count",
        dbs.cache
            ? t("storage.stat.clipboard_images", {
                count: dbs.cache.clipboard_image_count ?? 0,
                size: formatSize(dbs.cache.clipboard_image_size_bytes ?? 0),
            })
            : "-"
    );
}

function setText(id, text) {
    const el = document.getElementById(id);
    if (el) el.textContent = text;
}

// ── 0.16.6 完整清理 ──────────────────────────────────────────────────────

let _cachedCleanupInfo = null;

async function loadCleanupInfo() {
    try {
        _cachedCleanupInfo = await invoke("get_cleanup_info");
        renderCleanupInfo();
    } catch (e) {
        console.error("loadCleanupInfo failed:", e);
    }
}

function renderCleanupInfo() {
    if (!_cachedCleanupInfo) return;
    const info = _cachedCleanupInfo;

    setText("cleanup-data-dir-size", `${info.data_dir_size_mb?.toFixed(1) ?? "0"} MB`);
    setText("cleanup-db-size", `${info.db_total_mb?.toFixed(1) ?? "0"} MB`);
    setText("cleanup-logs-size", `${info.logs_size_mb?.toFixed(1) ?? "0"} MB`);
    setText("cleanup-python-size", `${info.python_size_mb?.toFixed(1) ?? "0"} MB`);
    setText("cleanup-skills-size", `${info.skills_size_mb?.toFixed(1) ?? "0"} MB`);
    setText(
        "cleanup-secret-count",
        t("storage.cleanup.secret_count", {count: info.secret_count ?? 0})
    );
}

function renderCleanupResults(result) {
    const el = document.getElementById("cleanup-results");
    if (!el || !result) return;

    const results = result.results || [];
    const parts = results.map((r) => {
        // P3-#25 fix: 用 Lucide SVG 图标替代 emoji（铁则：图标用包禁 emoji）
        const icon = r.success ? iconHTML("check") : iconHTML("x");
        // P3-#25 fix: 转义 r.error 和 r.target 防 XSS
        const detail = r.success ? "" : ` (${escapeHtml(r.error)})`;
        return `${icon} ${escapeHtml(r.target)}${detail}`;
    });

    const summary = t("storage.cleanup.summary", {
        success: result.success_count ?? 0,
        failed: result.failed_count ?? 0,
    });

    el.innerHTML = `<div class="cleanup-summary">${escapeHtml(summary)}</div><div class="cleanup-detail">${parts.join("<br>")}</div>`;
    el.hidden = false;
}

/** HTML 转义工具，防止 innerHTML XSS。 */
function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, (c) => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
    })[c]);
}

// 语言切换时重新渲染文本（带参数的 i18n 需要手动重渲染）
onLangChange(renderStorageInfo);
