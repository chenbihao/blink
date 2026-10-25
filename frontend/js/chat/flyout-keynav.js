/**
 * Flyout 键盘导航（0.27.15）——思考下拉 / 模型下拉 / 模型 flyout 共用。
 *
 * 仲裁约定（§6.5）：面板打开时 ↑/↓ 移动高亮、Enter/Tab 采纳、Esc 仅关面板；
 * IME 组合中（isComposing）不处理——中文输入法选字回车/方向键不误采纳。
 * 文档级监听（面板可能经按钮点击或 /model 命令打开，焦点位置不固定）；
 * 面板关闭时零开销短路。
 */

/** @type {Map<string, {container: () => HTMLElement|null, isActive: () => boolean, adopt: (el: HTMLElement) => void, close: () => void}>} */
const handlers = new Map();

let bound = false;

function ensureBound() {
    if (bound) return;
    bound = true;
    document.addEventListener("keydown", (e) => {
        if (e.isComposing) return; // IME 组合中不抢键盘
        for (const h of handlers.values()) {
            if (h.isActive() && handleOne(e, h)) return;
        }
    });
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
            e.stopPropagation();
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
            e.stopPropagation();
            if (active) h.adopt(active);
            else h.close();
            return true;
        }
        case "Escape": {
            e.preventDefault();
            e.stopPropagation();
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
