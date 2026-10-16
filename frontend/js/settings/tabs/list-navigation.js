import {invoke} from "../../shared/tauri.js";
import {saveConfig} from "../../shared/config-keys.js";
import {withConfigActivity} from "../../shared/config-sync.js";

/** 捕获变更边沿并串行保存；广播回填暂停到全部排队意图落定。 */
export function createWheelModeWriter({onError, onSaved}) {
    let confirmed = "step", chain = Promise.resolve();
    return {
        setConfirmed(mode) { confirmed = mode === "page" ? "page" : "step"; },
        save(mode) {
            const value = mode === "page" ? "page" : "step";
            return withConfigActivity("wheel_mode", () => {
                const pending = chain.then(async () => {
                    try {
                        await saveConfig("wheel_mode", value, {expected: confirmed});
                        confirmed = value;
                        onSaved(value);
                        onError(null);
                        return true;
                    } catch (error) {
                        console.error("save main-window wheel mode failed:", error);
                        try { confirmed = (await invoke("get_config")).wheel_mode; }
                        catch (readError) { console.error("reload wheel mode failed:", readError); }
                        onError(error);
                        return false;
                    }
                });
                chain = pending.catch(() => {});
                return pending;
            });
        },
    };
}
