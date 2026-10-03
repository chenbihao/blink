/** 配置读取失败保留错误与重试入口；不以默认配置解锁写入。 */
import {t} from "../i18n/index.js";
import {normalizeError} from "./tauri.js";

export function createConfigLoadNotice(container, retry, buttonClass = "btn-small") {
    let notice, message, button;
    return {
        show(error) {
            if (!container) return;
            if (!notice) {
                notice = document.createElement("div");
                notice.className = "setting-hint";
                notice.setAttribute("role", "alert");
                message = document.createElement("p");
                button = document.createElement("button");
                button.type = "button";
                button.className = buttonClass;
                button.addEventListener("click", async () => {
                    if (button.disabled) return;
                    button.disabled = true;
                    try { await retry(); }
                    finally { button.disabled = false; }
                });
                notice.append(message, button);
                container.prepend(notice);
            }
            message.textContent = t("config.load_failed", {err: normalizeError(error).message});
            button.textContent = t("config.reload");
            notice.hidden = false;
        },
        clear() { if (notice) notice.hidden = true; },
    };
}
