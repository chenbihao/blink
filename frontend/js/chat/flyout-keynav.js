/**
 * Flyout 键盘导航（0.27.15）——思考下拉 / 模型下拉 / 模型 flyout 共用。
 *
 * 仲裁约定（§6.5）：面板打开时 ↑/↓ 移动高亮、Enter/Tab 采纳、Esc 仅关面板；
 * IME 组合中（isComposing）不处理——中文输入法选字回车/方向键不误采纳。
 *
 * 两层路由（0.27.15 review 修正）：
 * - **composer target 阶段优先**（`handleFlyoutKeynav`）：焦点在输入框时，
 *   Enter/Tab 先给面板采纳（与命令面板同构），不再"发送消息 + 采纳条目"双动作。
 * - **document 级兜底**：焦点在按钮/body 时导航；可编辑目标（输入框/面板内嵌
 *   输入框）的非 Esc 键在此层放行——保持原生键行为（光标移动/换行），
 *   Esc 仍关面板。已消费按键 `stopImmediatePropagation`，防止同节点的
 *   后续监听（如 main.js handleEsc 关窗口）双重消费。
 */

/** @type {Map<string, {container: () => HTMLElement|null, isActive: () => boolean, adopt: (el: HTMLElement) => void, close: () => void}>} */
const handlers = new Map();

let bound = false;

/** 最近一次被本模块消费的 keydown 事件（供 handleEsc 等同节点监听仲裁）。 */
let lastConsumed = null;

function dispatchKeynav(e) {
    for (const h of handlers.values()) {
        if (h.isActive() && handleOne(e, h)) return true;
    }
    return false;
}

function isEditableTarget(target) {
    if (!target || typeof target.closest !== "function") return false;
    return Boolean(target.closest('input, textarea, [contenteditable="true"]'));
}

function ensureBound() {
    if (bound) return;
    bound = true;
    document.addEventListener("keydown", (e) => {
        if (e.isComposing) return; // IME 组合中不抢键盘
        // 可编辑目标的非 Esc 键不在 document 层处理：composer 已在 target 阶段
        // 优先路由；面板内嵌输入框（如自定义档位）保持原生键行为
        if (e.key !== "Escape" && isEditableTarget(e.target)) return;
        if (dispatchKeynav(e)) lastConsumed = e;
    });
}

/**
 * composer target 阶段路由入口——面板打开时先于 Enter 发送/换行消费导航键。
 * @param {KeyboardEvent} e
 * @returns {boolean} true 表示已被面板消费，composer 不应再处理
 */
export function handleFlyoutKeynav(e) {
    if (e.isComposing) return false;
    if (dispatchKeynav(e)) {
        lastConsumed = e;
        return true;
    }
    return false;
}

/**
 * 该事件是否已被 flyout 键盘导航消费（Esc 仲裁——main.js handleEsc 据此
 * 只关面板，不隐藏窗口/中止生成；与监听注册顺序无关）。
 * @param {KeyboardEvent} e
 * @returns {boolean}
 */
export function wasConsumedByKeynav(e) {
    return e === lastConsumed;
}

/**
 * 处理一个 flyout 的键盘事件；返回 true 表示已消费。
 * @param {KeyboardEvent} e
 * @param {{container: () => HTMLElement|null, adopt: (el: HTMLElement) => void, close: () => void}} h
 * @returns {boolean}
 */
function handleOne(e, h) {
    const container = h.container();
    if (!container || container.hidden) return false;
    // 可导航行 = 带 .flyout-nav-item 的元素（各模块渲染时标记）；禁用行跳过
    const items = Array.from(
        container.querySelectorAll(".flyout-nav-item")
    ).filter((el) => !el.classList.contains("flyout-nav-item-disabled"));
    if (items.length === 0) return false;
    const activeIndex = items.findIndex((el) =>
        el.classList.contains(KEY_ACTIVE_CLASS)
    );
    switch (e.key) {
        case "ArrowUp":
        case "ArrowDown": {
            e.preventDefault();
            e.stopImmediatePropagation();
            const delta = e.key === "ArrowDown" ? 1 : -1;
            const next =
                activeIndex === -1
                    ? (delta === 1 ? 0 : items.length - 1)
                    : (activeIndex + delta + items.length) % items.length;
            setKeyActive(items, next);
            return true;
        }
        case "Enter":
        case "Tab": {
            if (e.shiftKey) return false; // Shift+Enter/Tab 保持原生（换行等）
            const active =
                activeIndex === -1 ? null : items[activeIndex];
            e.preventDefault();
            e.stopImmediatePropagation();
            if (active) h.adopt(active);
            else h.close();
            return true;
        }
        case "Escape": {
            e.preventDefault();
            e.stopImmediatePropagation();
            h.close();
            return true;
        }
        default:
            return false;
    }
}

const KEY_ACTIVE_CLASS = "flyout-key-active";

function setKeyActive(items, index) {
    for (const el of items) el.classList.remove(KEY_ACTIVE_CLASS);
    if (index >= 0 && index < items.length) {
        items[index].classList.add(KEY_ACTIVE_CLASS);
        items[index].scrollIntoView({block: "nearest"});
    }
}

/**
 * 注册一个 flyout 的键盘导航。
 * @param {string} id 唯一标识（重复注册覆盖旧值）
 * @param {{container: () => HTMLElement|null, isActive: () => boolean, adopt: (el: HTMLElement) => void, close: () => void}} handler
 */
export function registerFlyoutKeynav(id, handler) {
    ensureBound();
    handlers.set(id, handler);
}

/** 清除高亮（面板关闭时调用，防下次打开残留）。 */
export function clearFlyoutKeyActive(container) {
    if (!container) return;
    container.querySelectorAll(`.${KEY_ACTIVE_CLASS}`).forEach((el) =>
        el.classList.remove(KEY_ACTIVE_CLASS)
    );
}
