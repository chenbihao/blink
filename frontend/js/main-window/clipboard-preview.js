import {invoke} from "../shared/tauri.js";
import {t} from "../i18n/index.js";

let panel;
let anchor;
let revision = 0;
let showTimer;
let hideTimer;
let previewImage;
let pendingAnchor;
let panelHovered = false;
let anchorHovered = false;
let selectingText = false;
let menuHold;

const SHOW_DELAY = 350;
const HIDE_DELAY = 150;

export function hideClipboardPreview() {
    cancelShow();
    closePanel();
}

function cancelShow() {
    clearTimeout(showTimer);
    showTimer = undefined;
    pendingAnchor = null;
}

function cancelHide() {
    clearTimeout(hideTimer);
    hideTimer = undefined;
}

function closePanel() {
    revision++;
    cancelHide();
    anchor?.removeAttribute("aria-describedby");
    anchor = null;
    menuHold = null;
    anchorHovered = panelHovered = selectingText = false;
    if (previewImage) {
        previewImage.onload = null;
        previewImage.onerror = null;
        previewImage.removeAttribute("src");
        previewImage = null;
    }
    if (panel) {
        if (document.activeElement === panel) document.getElementById("query")?.focus({preventScroll: true});
        panel.classList.add("hidden");
        panel.textContent = "";
        delete panel.dataset.errorText;
        panel.classList.remove("is-image-preview");
    }
}

function scheduleHide() {
    if (menuHold || selectingText || hideTimer !== undefined) return;
    // Closing the visible panel must not cancel another row's pending hover.
    hideTimer = setTimeout(closePanel, HIDE_DELAY);
}

/** Capture ownership and selection before the popup window can take focus. */
export function getClipboardPreviewContext(target) {
    if (!panel || !anchor?.isConnected || panel.classList.contains("hidden")
        || target.closest?.(".clipboard-text-preview") !== panel) return null;
    const owner = anchor, token = revision;
    const selection = window.getSelection();
    const selectedText = !previewImage && selection?.rangeCount && !selection.isCollapsed
        && panel.contains(selection.anchorNode) && panel.contains(selection.focusNode)
        ? selection.toString() : "";
    return {anchor: owner, selectedText,
        isCurrent: () => revision === token && anchor === owner && owner.isConnected};
}

/** Only the matching menu session may release the preview it retained. */
export function holdClipboardPreview(context) {
    if (!context?.isCurrent()) return () => {};
    const hold = {};
    menuHold = hold;
    selectingText = false;
    cancelHide();
    cancelShow();
    return () => {
        if (menuHold !== hold) return;
        menuHold = null;
        if (!panelHovered && !anchorHovered) scheduleHide();
    };
}

function ensurePanel() {
    if (panel) return panel;
    panel = document.createElement("div");
    panel.id = "clipboard-text-preview";
    panel.className = "clipboard-text-preview hidden";
    panel.setAttribute("role", "tooltip");
    panel.tabIndex = -1;
    panel.addEventListener("pointerenter", () => {
        panelHovered = true;
        cancelHide();
        cancelShow();
    });
    panel.addEventListener("pointerleave", () => {
        panelHovered = false;
        scheduleHide();
    });
    panel.addEventListener("pointerdown", event => {
        if (event.button !== 0) return;
        cancelHide();
        cancelShow();
        selectingText = true;
        panel.focus({preventScroll: true});
    });
    panel.addEventListener("keydown", event => {
        // Native selection/copy belongs to this panel, not clipboard row shortcuts.
        if (["Control", "Shift", "Alt", "Meta", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown"].includes(event.key)
            || ((event.ctrlKey || event.metaKey) && ["a", "c"].includes(event.key.toLowerCase()))) {
            event.stopPropagation();
            if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a") {
                event.preventDefault();
                window.getSelection()?.selectAllChildren(panel);
            }
        } else if (event.key === "Escape") {
            if (menuHold) return; // The menu owns the first Escape.
            event.preventDefault();
            event.stopPropagation();
            hideClipboardPreview();
        }
    });
    panel.addEventListener("wheel", event => event.stopPropagation());
    document.body.appendChild(panel);
    window.addEventListener("blur", () => {
        if (menuHold) { panelHovered = anchorHovered = false; return; }
        hideClipboardPreview();
    });
    document.addEventListener("keydown", () => { if (!menuHold) hideClipboardPreview(); });
    const finishSelection = () => {
        if (!selectingText) return;
        selectingText = false;
        if (!panelHovered && !anchorHovered) scheduleHide();
    };
    document.addEventListener("pointerup", finishSelection);
    document.addEventListener("pointercancel", finishSelection);
    return panel;
}

function placePanel() {
    if (!anchor?.isConnected || !panel) return;
    const rect = anchor.getBoundingClientRect();
    const width = Math.max(1, Math.min(rect.width, window.innerWidth - 16));
    panel.style.width = `${width}px`;
    const maxHeight = Math.max(1, Math.min(260, window.innerHeight - 16));
    panel.style.maxHeight = `${maxHeight}px`;
    panel.style.setProperty("--preview-image-height", `${Math.max(1, maxHeight - 22)}px`);
    panel.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - width - 8))}px`;
    const height = panel.getBoundingClientRect().height;
    const below = rect.bottom + 6;
    const top = below + height <= window.innerHeight - 8 ? below : rect.top - height - 6;
    panel.style.top = `${Math.max(8, Math.min(top, window.innerHeight - height - 8))}px`;
}

/** Fetch full text only after hover; stale responses never replace another entry's preview. */
export function bindClipboardPreview(li, ref) {
    if (ref?.kind === "text") bindPreview(li, {loadText: () => invoke("get_clipboard_text", {id: ref.id})});
    else if (ref?.kind === "image") bindPreview(li, {imageId: ref.id});
}

/** Errors reuse the bounded, scrollable preview without any additional IPC. */
export function bindErrorPreview(li, text) {
    bindPreview(li, {loadText: () => text, isError: true});
}

function bindPreview(li, {loadText, isError = false, imageId}) {
    const leaveTrigger = () => {
        if (pendingAnchor === li) cancelShow();
        if (anchor !== li) return;
        anchorHovered = false;
        scheduleHide();
    };
    const onPointer = event => {
        if (menuHold || selectingText) return;
        // 文本与图片统一左侧三分之一触发，右侧保留给点击/拖选等列表操作。
        const rect = li.getBoundingClientRect();
        const inTrigger = Number.isFinite(event.clientX) && event.clientX >= rect.left
            && event.clientX <= rect.left + rect.width / 3;
        if (!inTrigger) {
            leaveTrigger();
            return;
        }
        // Dragging or selecting rows should never open a preview.
        if (event.buttons) { if (anchor === li) hideClipboardPreview(); return; }
        if (anchor === li) {
            anchorHovered = true;
            cancelHide();
            cancelShow();
            return;
        }
        if (pendingAnchor === li) return;
        cancelShow();
        pendingAnchor = li;
        if (anchor) scheduleHide();
        showTimer = setTimeout(async () => {
            if (pendingAnchor !== li || !li.isConnected) return;
            cancelShow();
            closePanel();
            anchor = li;
            anchorHovered = true;
            const token = revision;
            const view = ensurePanel();
            view.textContent = t(imageId ? "clipboard.preview_image_loading" : "clipboard.preview_loading");
            view.scrollTop = 0;
            view.classList.remove("hidden");
            li.setAttribute("aria-describedby", view.id);
            placePanel();
            if (imageId) {
                const image = document.createElement("img");
                previewImage = image;
                image.className = "clipboard-preview-image";
                image.alt = li.querySelector("img.clip-thumb")?.alt || "";
                image.decoding = "async";
                image.onload = () => {
                    if (token !== revision || anchor !== li || !li.isConnected) return;
                    view.classList.add("is-image-preview");
                    view.replaceChildren(image);
                    placePanel();
                };
                image.onerror = () => {
                    if (token !== revision || anchor !== li || !li.isConnected) return;
                    view.textContent = t("clipboard.preview_missing");
                    placePanel();
                };
                image.src = "http://blink-clipimg.localhost/preview/" + encodeURIComponent(imageId);
                return;
            }
            try {
                const text = await loadText();
                if (token !== revision || anchor !== li || !li.isConnected) return;
                view.textContent = typeof text === "string" ? text : t("clipboard.preview_missing");
                if (isError && typeof text === "string") view.dataset.errorText = text;
                else delete view.dataset.errorText;
                placePanel();
            } catch {
                if (token !== revision || anchor !== li || !li.isConnected) return;
                view.textContent = t("clipboard.preview_missing");
                placePanel();
            }
        }, SHOW_DELAY);
    };
    li.addEventListener("pointerenter", onPointer);
    li.addEventListener("pointermove", onPointer);
    li.addEventListener("pointerdown", hideClipboardPreview);
    li.addEventListener("wheel", hideClipboardPreview);
    li.addEventListener("pointerleave", leaveTrigger);
}
