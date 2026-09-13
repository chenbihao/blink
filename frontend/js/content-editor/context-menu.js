import {copyToClipboard} from "../shared/api.js";
import {t} from "../i18n/index.js";

/** 右键只提供当前正文的编辑动作；整理沿用已有选区候选链路。 */
export function editorContextItems({hasSelection, hasMeaningfulSelection, hasText, editable, aiAvailable, transformIdle}) {
    const items = [];
    if (hasSelection) {
        if (editable) items.push("cut");
        items.push("copy");
    }
    if (editable) items.push("paste");
    if (hasText) items.push("selectAll");
    if (hasMeaningfulSelection && editable && aiAvailable && transformIdle) {
        items.push("separator", "tidySelection");
    }
    return items;
}

/** 菜单挂在根节点，避开正文 overflow:hidden，同时限制在编辑窗口内。 */
export function clampEditorMenuPosition(x, y, menuWidth, menuHeight, rootWidth, rootHeight) {
    return {
        x: Math.max(4, Math.min(x, Math.max(4, rootWidth - menuWidth - 4))),
        y: Math.max(4, Math.min(y, Math.max(4, rootHeight - menuHeight - 4))),
    };
}

export function bindEditorContextMenu({root, source, markdown, adapter, session, transform, setStatus, closeMoreMenu}) {
    let menu = null;

    function close() {
        menu?.remove();
        menu = null;
    }

    function isEditorTarget(target) {
        return target === source || markdown?.contains(target);
    }

    function selectionSignature() {
        if (adapter.view === "source") {
            return `${source.selectionStart}:${source.selectionEnd}`;
        }
        const selection = adapter.engine?.editor?.state?.selection;
        return selection ? `${selection.from}:${selection.to}` : "";
    }

    async function run(action, identity) {
        close();
        if (!session.isActive || session.sessionRef !== identity.sessionRef
            || session.generation !== identity.generation || adapter.view !== identity.view) return;
        if (adapter.revision !== identity.revision || selectionSignature() !== identity.selection) return;

        if (action === "tidySelection") {
            if (!adapter.getSelectionText().trim() || !transform.aiAvailable
                || transform.isBusy || transform.candidate) return;
            void transform.start("selection");
            return;
        }
        if (action === "selectAll") {
            if (adapter.view === "source") {
                source.focus();
                source.select();
            } else {
                adapter.engine?.editor?.commands?.selectAll();
                adapter.focus();
            }
            return;
        }
        if (action === "copy") {
            const selected = adapter.getSelectionText();
            if (selected) await copyToClipboard(selected);
            return;
        }
        if (!adapter.isEditable()) return;
        if (action === "cut") {
            if (!adapter.getSelectionText()) return;
            adapter.focus();
            if (!document.execCommand("cut")) throw new Error("cut failed");
            return;
        }
        if (action === "paste") {
            const revision = adapter.revision;
            const signature = selectionSignature();
            let text;
            try {
                text = await navigator.clipboard.readText();
            } catch {
                if (!session.isActive || session.sessionRef !== identity.sessionRef
                    || session.generation !== identity.generation || adapter.view !== identity.view
                    || adapter.revision !== revision || selectionSignature() !== signature
                    || !document.hasFocus()) return;
                adapter.focus();
                if (!document.execCommand("paste")) throw new Error("paste failed");
                return;
            }
            // 剪贴板读取会异步返回；其间若正文或插入点改变，不能写到新位置。
            if (!session.isActive || session.sessionRef !== identity.sessionRef
                || session.generation !== identity.generation || adapter.view !== identity.view
                || adapter.revision !== revision || selectionSignature() !== signature
                || !document.hasFocus()) return;
            adapter.focus();
            if (text && !document.execCommand("insertText", false, text)) {
                throw new Error("paste failed");
            }
        }
    }

    root.addEventListener("contextmenu", (event) => {
        if (!isEditorTarget(event.target)) return;
        event.preventDefault();
        close();
        if (!session.isActive || document.querySelector(".modal-overlay")) return;
        closeMoreMenu?.();
        const selected = adapter.getSelectionText();
        const items = editorContextItems({
            hasSelection: !!selected,
            hasMeaningfulSelection: !!selected.trim(),
            hasText: !!adapter.getText(),
            editable: adapter.isEditable(),
            aiAvailable: transform.aiAvailable,
            transformIdle: !transform.isBusy && !transform.candidate,
        });
        if (!items.length) return;
        const identity = {
            sessionRef: session.sessionRef, generation: session.generation, view: adapter.view,
            revision: adapter.revision, selection: selectionSignature(),
        };
        const labels = {
            cut: t("menu.cut"), copy: t("menu.copy"), paste: t("menu.paste"),
            selectAll: t("menu.selectAll"),
            tidySelection: t("editor.transform.tidySelection", {count: selected.length}),
        };
        menu = document.createElement("div");
        menu.className = "editor-context-menu";
        menu.setAttribute("role", "menu");
        menu.setAttribute("aria-label", t("editor.more"));
        for (const action of items) {
            if (action === "separator") {
                const separator = document.createElement("div");
                separator.className = "editor-context-separator";
                separator.setAttribute("role", "separator");
                menu.appendChild(separator);
                continue;
            }
            const button = document.createElement("button");
            button.type = "button";
            button.setAttribute("role", "menuitem");
            button.textContent = labels[action];
            // 鼠标点击菜单时保留 textarea/ProseMirror 的焦点和选区。
            button.addEventListener("mousedown", (e) => e.preventDefault());
            button.addEventListener("click", () => {
                void run(action, identity).catch((error) => {
                    console.error("[content-editor] 右键动作失败:", error);
                    setStatus(t("editor.actionFailed", {message: String(error)}));
                });
            });
            menu.appendChild(button);
        }
        root.appendChild(menu);
        menu.addEventListener("keydown", (e) => {
            const buttons = [...menu.querySelectorAll("button")];
            const current = buttons.indexOf(document.activeElement);
            if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                e.preventDefault();
                const step = e.key === "ArrowDown" ? 1 : -1;
                buttons[(current + step + buttons.length) % buttons.length]?.focus();
            } else if (e.key === "Home" || e.key === "End") {
                e.preventDefault();
                buttons[e.key === "Home" ? 0 : buttons.length - 1]?.focus();
            }
        });
        const rootRect = root.getBoundingClientRect();
        const rect = menu.getBoundingClientRect();
        const point = clampEditorMenuPosition(
            event.clientX - rootRect.left, event.clientY - rootRect.top,
            rect.width, rect.height, rootRect.width, rootRect.height,
        );
        menu.style.left = `${point.x}px`;
        menu.style.top = `${point.y}px`;
        if (event.button === -1) menu.querySelector("button")?.focus();
    });

    document.addEventListener("pointerdown", (event) => {
        if (menu && !menu.contains(event.target)) close();
    });
    window.addEventListener("blur", close);
    document.addEventListener("keydown", (event) => {
        if (!menu || event.key !== "Escape") return;
        event.preventDefault();
        event.stopImmediatePropagation();
        close();
        adapter.focus();
    }, true);
    return close;
}
