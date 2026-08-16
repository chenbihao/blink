//! 国际化（i18n）：中英文 UI 文本切换。
//!
//! 纯静态前端、无 bundler，故手写轻量字典方案：
//! - 静态 HTML 文本：元素打 data-i18n / data-i18n-ph / data-i18n-title，applyI18n() 遍历覆盖。
//! - JS 动态文本：调用方直接 t(key, params)（右键菜单、提示栏、卡片动态渲染等）。
//!
//! 语言来源：AppConfig.language（"zh" | "en"），由 applyI18nFromConfig() 异步读取。
//! 降级：t() 找不到 key → 回退 zh → 再回退到 key 本身，永不抛异常。

import {invoke} from "../shared/tauri.js";
import {zh} from "./zh.js";
import {en} from "./en.js";

/** 当前语言（模块私有）。setLang 修改，t/applyI18n 读取。 */
let currentLang = "zh";

/** zh / en 双语字典。扁平点分 key，两份必须 key 对齐（缺 key 靠 t() 回退兜底）。 */
const DICT = {zh, en};

/** 支持的语言集合（setLang 合法性校验用）。 */
const SUPPORTED = new Set(["zh", "en"]);

/**
 * 取翻译并插值。{name} 占位符用 params[name] 替换（缺失填空串）。
 * 降级链：currentLang → zh → key 本身。永不抛异常。
 * @param {string} key 点分字典 key，如 "menu.copy"
 * @param {Record<string, string|number>} [params] 插值参数
 * @returns {string}
 */
export function t(key, params) {
    const raw = DICT[currentLang]?.[key] ?? DICT.zh[key] ?? key;
    if (!params) return raw;
    return raw.replace(/\{(\w+)\}/g, (_, name) => (params[name] ?? "").toString());
}

/** 读取当前语言。 */
export function getLang() {
    return currentLang;
}

/**
 * 设置当前语言（仅改模块状态，不刷 DOM）。非法值回退 zh。
 * @param {string} lang "zh" | "en"
 */
export function setLang(lang) {
    currentLang = SUPPORTED.has(lang) ? lang : "zh";
}

/** 语言切换订阅者。applyI18n 刷完静态文本后触发，供 JS 动态生成的本地化文本（如状态徽章）自行刷新。 */
const langChangeCallbacks = new Set();

/**
 * 订阅语言切换。回调在 applyI18n 末尾执行（此时静态 DOM 文本已更新）。
 * @param {() => void} cb
 * @returns {() => void} 取消订阅函数
 */
export function onLangChange(cb) {
    langChangeCallbacks.add(cb);
    return () => langChangeCallbacks.delete(cb);
}

const ATTRS = [
    ["data-i18n", "textContent"],
    ["data-i18n-ph", "placeholder"],
    ["data-i18n-title", "title"],
    ["data-i18n-aria-label", "ariaLabel"],
    // data-tip 存的是 i18n key，翻译后写入 dataset.tip（CSS tooltip::attr(data-tip) 读它）。
    // 特殊：不能用 el[prop] 直接映射，dataset 是只读对象，需走 setAttribute。
    ["data-tip", "__dataset_tip__"],
];

/**
 * 遍历 DOM 中打了 i18n 标记的元素，按当前语言批量覆盖文本/属性。
 * 带插值的动态文本（如计数、翻页）不能用此法，须调用方自行 t() 重算。
 *
 * - `data-i18n` / `data-i18n-ph` / `data-i18n-title` / `data-i18n-aria-label`：
 *   纯文本覆盖（textContent / placeholder / title / ariaLabel）。
 * - `data-i18n-html`：翻译串可含受信任的 HTML（如 <code>/<strong>，来自本地字典而非用户输入），
 *   走 innerHTML 渲染。仅用于静态说明文案；动态用户内容绝不走此属性。
 *
 * @param {string} [lang] 不传则用 currentLang（传入时也会 setLang）
 */
export function applyI18n(lang) {
    if (lang) setLang(lang);
    for (const [attr, prop] of ATTRS) {
        document.querySelectorAll(`[${attr}]`).forEach((el) => {
            if (prop === "textContent" && el.childElementCount > 0) {
                // 保留子元素（如 field-hint-icon），只更新第一个文本节点
                const text = t(el.getAttribute(attr));
                let textNode = [...el.childNodes].find((n) => n.nodeType === Node.TEXT_NODE);
                if (textNode) {
                    textNode.textContent = text;
                } else {
                    el.insertBefore(document.createTextNode(text), el.firstChild);
                }
            } else if (prop === "__dataset_tip__") {
                // data-tip 翻译后写回 data-tip 属性（CSS ::after content:attr(data-tip) 读它）
                el.setAttribute("data-tip", t(el.getAttribute(attr)));
            } else {
                el[prop] = t(el.getAttribute(attr));
            }
        });
    }
    // data-i18n-html：翻译串可含受信任 HTML，走 innerHTML
    document.querySelectorAll("[data-i18n-html]").forEach((el) => {
        el.innerHTML = t(el.getAttribute("data-i18n-html"));
    });
    // 静态文本刷完后，通知动态文本（状态徽章等 JS 设的 textContent）自行刷新
    for (const cb of langChangeCallbacks) {
        try {
            cb();
        } catch (e) {
            console.error("onLangChange callback failed:", e);
        }
    }
}

/**
 * 从 AppConfig 读 language 并应用：setLang + applyI18n。
 * 启动时与窗口 shown 刷新时调用。读失败保持 currentLang 默认值。
 */
export async function applyI18nFromConfig() {
    try {
        const cfg = await invoke("get_config");
        if (cfg && cfg.language) setLang(cfg.language);
    } catch (e) {
        console.error("applyI18nFromConfig: 读 config 失败，回退默认语言", e);
    }
    applyI18n();
}

/**
 * 从已读好的 config 对象应用语言（避免重复 invoke get_config）。
 * @param {object} cfg - get_config 返回的 AppConfig 对象
 */
export function applyI18nFromConfigData(cfg) {
    if (cfg && cfg.language) setLang(cfg.language);
    applyI18n();
}
