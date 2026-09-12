/**
 * 恢复冲突专用对话框（四动作）。
 *
 * `choiceDialog` 是三态组件，且把 Esc / 遮罩点击折叠为 cancel——恢复冲突
 * 场景需要四个语义互不相同的出口，复用三态返回值会把"关闭弹窗"误当
 * "复制草稿"（曾经的真实缺陷：用户按 Esc 导致敏感正文进剪贴板 + 候选被清）。
 *
 * 出口语义（严格区分）：
 * - "keep"    保留当前正文（用户显式点击；随后精确清理旧候选 + flush 当前）
 * - "restore" 恢复草稿到编辑器（显式点击）
 * - "copy"    复制草稿正文（**必须**显式点击；绝不因关闭/Esc 触发）
 * - "dismiss" 暂不处理（关闭按钮 / Esc / 遮罩点击 / 任何异常 fallback：
 *             当前正文与候选草稿都保持原样，无剪贴板写入、无清理）
 *
 * DOM 结构复用 .modal-overlay / .confirm-dialog 现有样式；不进 shared/tauri.js
 * ——该组件语义只属于内容编辑器的恢复流程。
 */

/** kind → 图标 SVG（与 shared/tauri.js showCustomDialog 同源） */
const DIALOG_ICONS = {
    warning: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>`,
    info: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>`,
};

/**
 * 显示四动作恢复冲突对话框。
 * @param {object} opts
 * @param {string} opts.message - 正文说明（已翻译）
 * @param {string} [opts.title]
 * @param {"warning"|"info"} [opts.kind]
 * @param {{keepCurrent: string, restore: string, copy: string, dismiss: string}} opts.labels
 * @returns {Promise<"keep"|"restore"|"copy"|"dismiss">} 异常/异常路径恒为 "dismiss"
 */
export function showRecoveryConflictDialog({message, title, kind = "warning", labels}) {
    try {
        // executor 内的异常会变成 rejection（外层 catch 接不到），必须再
        // 挂一层 catch：任何构建/渲染失败都折叠为安全的 "dismiss"。
        return Promise.resolve(buildDialog({message, title, kind, labels})).catch((e) => {
            console.error("[recovery-dialog] 构建失败:", e);
            return "dismiss";
        });
    } catch (e) {
        console.error("[recovery-dialog] 构建失败:", e);
        return Promise.resolve("dismiss");
    }
}

function buildDialog({message, title, kind = "warning", labels}) {
    return new Promise((resolve) => {
        const icon = DIALOG_ICONS[kind] ?? DIALOG_ICONS.warning;

        const overlay = document.createElement("div");
        overlay.className = "modal-overlay confirm-dialog-overlay";

        const card = document.createElement("div");
        card.className = `confirm-dialog confirm-dialog-${kind} recovery-conflict-dialog`;

        const iconEl = document.createElement("div");
        iconEl.className = "confirm-dialog-icon";
        iconEl.innerHTML = icon;

        const contentEl = document.createElement("div");
        contentEl.className = "confirm-dialog-content";
        const titleEl = document.createElement("div");
        titleEl.className = "confirm-dialog-title";
        titleEl.textContent = title ?? "";
        const msgEl = document.createElement("div");
        msgEl.className = "confirm-dialog-message";
        msgEl.textContent = message ?? "";
        contentEl.appendChild(titleEl);
        contentEl.appendChild(msgEl);

        const headerEl = document.createElement("div");
        headerEl.className = "confirm-dialog-header";
        headerEl.appendChild(iconEl);
        headerEl.appendChild(contentEl);

        const actionsEl = document.createElement("div");
        actionsEl.className = "confirm-dialog-actions recovery-conflict-actions";

        let resolved = false;
        const cleanup = () => {
            document.removeEventListener("keydown", onKey, true);
        };
        const finish = (result) => {
            if (resolved) return;
            resolved = true;
            cleanup();
            overlay.classList.add("confirm-dialog-closing");
            setTimeout(() => overlay.remove(), 150);
            resolve(result);
        };

        // 关闭/暂不处理：独立按钮，Esc 与遮罩点击等价于它（都不产生副作用）
        const dismissBtn = document.createElement("button");
        dismissBtn.type = "button";
        dismissBtn.className = "btn btn-small";
        dismissBtn.textContent = labels.dismiss;
        dismissBtn.addEventListener("click", () => finish("dismiss"));
        actionsEl.appendChild(dismissBtn);

        // 复制草稿：必须显式点击（危险出口：写剪贴板）
        const copyBtn = document.createElement("button");
        copyBtn.type = "button";
        copyBtn.className = "btn btn-small";
        copyBtn.textContent = labels.copy;
        copyBtn.addEventListener("click", () => finish("copy"));
        actionsEl.appendChild(copyBtn);

        // 保留当前：warning 语义下的危险配色（会清理旧候选）
        const keepBtn = document.createElement("button");
        keepBtn.type = "button";
        keepBtn.className = "btn btn-danger";
        keepBtn.textContent = labels.keepCurrent;
        keepBtn.addEventListener("click", () => finish("keep"));
        actionsEl.appendChild(keepBtn);

        // 恢复草稿：主出口（Enter）
        const restoreBtn = document.createElement("button");
        restoreBtn.type = "button";
        restoreBtn.className = "btn-primary";
        restoreBtn.textContent = labels.restore;
        restoreBtn.addEventListener("click", () => finish("restore"));
        actionsEl.appendChild(restoreBtn);

        card.appendChild(headerEl);
        card.appendChild(actionsEl);
        overlay.appendChild(card);

        // 遮罩点击 = 暂不处理（与显式关闭按钮同语义，绝不当复制）
        overlay.addEventListener("click", (e) => {
            if (e.target === overlay) finish("dismiss");
        });

        const onKey = (e) => {
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                finish("dismiss");
            } else if (e.key === "Enter") {
                e.preventDefault();
                e.stopPropagation();
                finish("restore");
            }
        };
        document.addEventListener("keydown", onKey, true);

        document.body.appendChild(overlay);
        requestAnimationFrame(() => restoreBtn.focus());
    });
}
