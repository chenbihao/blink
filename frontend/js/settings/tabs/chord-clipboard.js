import {invoke} from "../../shared/tauri.js";
import {saveConfig} from "../../shared/config-keys.js";
import {withConfigActivity} from "../../shared/config-sync.js";

/** 变更边沿捕获字段值；写入排队期间暂停设置页回填，避免旧 DOM/整片快照覆盖新意图。 */
export function createClipboardConfigWriter({onError}) {
    let confirmed = {}, chain = Promise.resolve();
    return {
        setConfirmed(cfg) { confirmed = structuredClone(cfg || {}); },
        save(control) {
            const field = control.dataset.field;
            let value;
            if (field === "search_enabled") value = control.checked;
            else if (field === "blacklist_keywords") value = control.value.split(",").map(s => s.trim()).filter(Boolean);
            else {
                value = Number(control.value);
                if (!control.value.trim() || !Number.isInteger(value) || !control.checkValidity()) {
                    control.reportValidity();
                    return Promise.resolve(false);
                }
            }
            return withConfigActivity("clipboard_config", () => {
                const pending = chain.then(async () => {
                    try {
                        const expected = {[field]: confirmed[field]};
                        await saveConfig("clipboard_config", {[field]: value}, {expected});
                        confirmed[field] = value;
                        onError("");
                        return true;
                    } catch (error) {
                        console.error("save clipboard field failed:", error);
                        // 恢复权威基线后才处理后续排队意图，错误保留在列表外。
                        try { confirmed = (await invoke("get_config")).clipboard; }
                        catch (readError) { console.error("reload clipboard config failed:", readError); }
                        onError(error?.message || String(error));
                        return false;
                    }
                });
                chain = pending.catch(() => {});
                return pending;
            });
        },
    };
}
