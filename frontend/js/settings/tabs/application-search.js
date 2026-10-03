/** 应用搜索偏好与后台发现状态；读取单飞、写入按字段 CAS。 */
import {invoke, listen, messageDialog} from "../../shared/tauri.js";
import {EVENTS} from "../../shared/event-names.js";
import {saveConfig} from "../../shared/config-keys.js";
import {createConfigRefresher} from "../../shared/config-sync.js";
import {onLangChange, t} from "../../i18n/index.js";

export function initApplicationSearch() {
    const controls = new Map([
        ["enabled", document.getElementById("start-menu-enabled")],
        ["scan_depth", document.getElementById("start-menu-scan-depth")],
        ["include_uwp", document.getElementById("start-menu-include-uwp")],
        ["include_system_shortcuts", document.getElementById("start-menu-include-system")],
        ["discover_system_settings", document.getElementById("start-menu-discover-settings")],
    ]);
    const badge = document.getElementById("system-entry-status");
    const retry = document.getElementById("system-entry-retry");
    let confirmed = null, pending = false, status = null, timer = null, statusGeneration = 0;
    function renderStatus() {
        const state = confirmed?.enabled && confirmed?.discover_system_settings ? status?.state || "building" : "disabled";
        badge.textContent = t(`engine.start_menu.discovery.${state}`, {count: status?.extra_count || 0});
        badge.className = `status-badge ${state === "ready" ? "status-available" : state === "failed" ? "status-unavailable" : "status-unknown"}`;
        retry.disabled = pending || !confirmed?.enabled || !confirmed?.discover_system_settings || state === "building";
    }
    function disableControls() {
        for (const [key, el] of controls) el.disabled = !confirmed || pending || (key !== "enabled" && !confirmed.enabled);
        document.querySelectorAll("#start-menu-options .number-spinner button").forEach(el => { el.disabled = !confirmed?.enabled || pending; });
        renderStatus();
    }
    function cancelStatus() { clearTimeout(timer); timer = null; statusGeneration++; }
    async function refreshStatus() {
        cancelStatus();
        if (!confirmed?.enabled || !confirmed?.discover_system_settings || document.hidden) { renderStatus(); return; }
        const revision = statusGeneration;
        try {
            const value = await invoke("get_system_entry_status");
            if (revision !== statusGeneration) return;
            status = value; renderStatus();
            if (status.state === "building") timer = setTimeout(refreshStatus, 1000);
        } catch (error) {
            if (revision !== statusGeneration) return;
            console.error("get_system_entry_status failed:", error);
            status = {state: "failed"}; renderStatus();
        }
    }
    const refresher = createConfigRefresher({
        read: () => invoke("get_start_menu_config"),
        apply(config) {
            confirmed = config;
            for (const [key, el] of controls) { if (key === "scan_depth") el.value = config[key]; else el.checked = config[key]; }
            disableControls(); refreshStatus();
        },
        onError(error) { console.error("get_start_menu_config failed:", error); disableControls(); },
    });
    disableControls();
    for (const [key, el] of controls) el.addEventListener("change", async () => {
        if (!confirmed || pending) return;
        const value = key === "scan_depth" ? Number(el.value) : el.checked;
        if (key === "scan_depth" && (!Number.isInteger(value) || value < 1 || value > 10)) { el.value = confirmed[key]; return; }
        const release = refresher.hold();
        pending = true; disableControls(); cancelStatus();
        try {
            await saveConfig("start_menu_config", {[key]: value}, {expected: {[key]: confirmed[key]}});
            confirmed = {...confirmed, [key]: value};
        } catch (error) {
            if (key === "scan_depth") el.value = confirmed[key]; else el.checked = confirmed[key];
            await messageDialog(t("common.save_failed_msg", {err: error}), {title: t("common.error"), kind: "error"});
        } finally { pending = false; disableControls(); release(); }
    });
    retry.addEventListener("click", async () => {
        retry.disabled = true;
        try { await invoke("refresh_system_entries"); await refreshStatus(); }
        catch (error) { console.error("refresh_system_entries failed:", error); status = {state: "failed"}; renderStatus(); }
    });
    window.addEventListener("focus", () => refresher.refresh());
    document.addEventListener("visibilitychange", () => { if (document.hidden) cancelStatus(); else refresher.refresh(); });
    onLangChange(() => { renderStatus(); refresher.refresh(); });
    listen(EVENTS.CONFIG_CHANGED, ({payload}) => {
        if (!payload?.key || ["engine:start_menu", "start_menu_config", "app.appearance"].includes(payload.key)) refresher.refresh();
    }).catch(console.error).finally(() => refresher.refresh());
}
