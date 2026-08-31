/**
 * 设置页统一深链导航。
 *
 * 所有 panel 共用 .content 滚动容器。跨 Tab 跳转必须先激活目标 Tab、
 * 清除上一个 panel 遗留的 scrollTop，再等待异步内容就绪并定位目标。
 */

let navigationRevision = 0;

function resolveTarget(target, documentRef) {
    if (!target) return null;
    if (typeof target === "function") return target();
    if (typeof target === "string") return documentRef.querySelector(target);
    return target;
}

export function resetSettingsContentScroll(documentRef = document) {
    const content = documentRef.querySelector(".content");
    if (content) content.scrollTop = 0;
}

/** 每次 Tab 激活都推进 revision，使尚未完成的旧深链失效。 */
export function markSettingsTabActivation() {
    navigationRevision++;
}

function activateSettingsTab(tabId, documentRef = document) {
    const tab = Array.from(documentRef.querySelectorAll(".tab"))
        .find((candidate) => candidate.dataset.tab === tabId);
    if (!tab) return false;
    tab.click();
    return true;
}

export function waitForSettingsTarget(target, {
    documentRef = document,
    timeoutMs = 2000,
} = {}) {
    const immediate = resolveTarget(target, documentRef);
    if (immediate || !target) return Promise.resolve(immediate);

    const view = documentRef.defaultView;
    const Observer = view?.MutationObserver;
    if (!Observer) return Promise.resolve(null);

    return new Promise((resolve) => {
        const observer = new Observer(() => {
            const element = resolveTarget(target, documentRef);
            if (!element) return;
            observer.disconnect();
            view.clearTimeout(timer);
            resolve(element);
        });
        observer.observe(documentRef.body, {childList: true, subtree: true});
        const timer = view.setTimeout(() => {
            observer.disconnect();
            resolve(resolveTarget(target, documentRef));
        }, timeoutMs);
    });
}

/**
 * 激活设置 Tab，并在可选异步准备完成后定位、聚焦目标。
 * target / focusTarget 支持 CSS selector、HTMLElement 或延迟求值函数。
 */
export async function navigateSettings({
    tabId,
    target = null,
    focusTarget = null,
    prepare = null,
    behavior = "smooth",
    block = "start",
    timeoutMs = 2000,
    documentRef = document,
} = {}) {
    if (!activateSettingsTab(tabId, documentRef)) {
        return {activated: false, target: null};
    }
    // Tab click handler 已推进 revision；之后任意 Tab 点击都会使本次导航过期。
    const revision = navigationRevision;

    // click handler 与这里都归零：前者覆盖手动/后端 eval 切 Tab，后者保证 helper 独立可靠。
    resetSettingsContentScroll(documentRef);
    if (prepare) await prepare();
    if (revision !== navigationRevision) {
        return {activated: true, target: null, stale: true};
    }

    const element = await waitForSettingsTarget(target, {documentRef, timeoutMs});
    if (revision !== navigationRevision) {
        return {activated: true, target: null, stale: true};
    }
    element?.scrollIntoView({behavior, block});

    const focusElement = resolveTarget(focusTarget, documentRef);
    focusElement?.focus({preventScroll: true});
    return {activated: true, target: element};
}
