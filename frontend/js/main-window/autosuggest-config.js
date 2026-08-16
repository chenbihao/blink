//! Autosuggestion 前端配置缓存（0.8.1 §2.8）。
//!
//! main.js 启动时 fetch 一次 AppConfig 里的 autosuggest_tab_key（`"Tab"` / `"ArrowRight"`），
//! 缓存到内存供 keyboard.js 判断"接受补全键"。设置页保存后可主动调 `refresh()`
//! 更新（也可以直接 setKey()——设置页知道用户选了什么）。

import {invoke} from "../shared/tauri.js";

let tabKey = "Tab"; // 默认

/** 启动时拉一次配置。 */
export async function init() {
    try {
        const config = await invoke("get_config");
        if (config && typeof config.autosuggest_tab_key === "string") {
            tabKey = config.autosuggest_tab_key;
        }
    } catch (e) {
        console.warn("autosuggest-config init failed, use default Tab:", e);
    }
}

/** 设置页保存后直接同步（比再 fetch 快一步）。 */
export function setKey(key) {
    if (key === "Tab" || key === "ArrowRight") {
        tabKey = key;
    }
}

/** keyboard.js 消费点。 */
export function getTabKey() {
    return tabKey;
}
