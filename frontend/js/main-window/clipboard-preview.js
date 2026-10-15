import {invoke} from "../shared/tauri.js";
import {t} from "../i18n/index.js";

let panel;
let anchor;
let revision = 0;
let showTimer;
let hideTimer;

export function hideClipboardPreview() {
    revision++;
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    anchor?.removeAttribute("aria-describedby");
    anchor = null;
    if (panel) {
        panel.classList.add("hidden");
        panel.textContent = "";
        delete panel.dataset.errorText;
    }
}

function scheduleHide() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(hideClipboardPreview, 150);
}

function ensurePanel() {
    if (panel) return panel;
    panel = document.createElement("div");
    panel.id = "clipboard-text-preview";
    panel.className = "clipboard-text-preview hidden";
    panel.setAttribute("role", "tooltip");
    panel.addEventListener("pointerenter", () => clearTimeout(hideTimer));
    panel.addEventListener("pointerleave", scheduleHide);
    panel.addEventListener("wheel", event => event.stopPropagation());
    document.body.appendChild(panel);
    window.addEventListener("blur", hideClipboardPreview);
    document.addEventListener("keydown", hideClipboardPreview);
    return panel;
}

function placePanel() {
    if (!anchor?.isConnected || !panel) return;
    const rect = anchor.getBoundingClientRect();
    const width = Math.min(560, window.innerWidth - 16);
    panel.style.width = `${width}px`;
    panel.style.maxHeight = `${Math.min(260, window.innerHeight - 16)}px`;
    panel.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - width - 8))}px`;
    const height = panel.getBoundingClientRect().height;
    const below = rect.bottom + 6;
    const top = below + height <= window.innerHeight - 8 ? below : rect.top - height - 6;
    panel.style.top = `${Math.max(8, Math.min(top, window.innerHeight - height - 8))}px`;
}

/** Fetch full text only after hover; stale responses never replace another entry's preview. */
export function bindClipboardPreview(li, ref) {
    if (ref?.kind !== "text") return;
    bindTextPreview(li, () => invoke("get_clipboard_text", {id: ref.id}));
}

/** Errors reuse the bounded, scrollable preview without any additional IPC. */
export function bindErrorPreview(li, text) {
    bindTextPreview(li, () => text, true);
}

function bindTextPreview(li, loadText, isError = false) {
    li.addEventListener("pointerenter", () => {
        if (anchor === li) { clearTimeout(hideTimer); return; }
        hideClipboardPreview();
        anchor = li;
        const token = revision;
        showTimer = setTimeout(async () => {
            if (token !== revision || !li.isConnected) return;
            const view = ensurePanel();
            view.textContent = t("clipboard.preview_loading");
            view.scrollTop = 0;
            view.classList.remove("hidden");
            li.setAttribute("aria-describedby", view.id);
            placePanel();
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
        }, 350);
    });
    li.addEventListener("pointerleave", scheduleHide);
}
