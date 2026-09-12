/**
 * EditorActions（0.23.2）——保存目标显示与次级动作的唯一装配点。
 *
 * 职责（phase 文档 §3.5/§3.8）：
 * - 主保存区真实去向渲染（CommitTarget 快照/提交结果驱动）；
 * - "更多"菜单：保存到…/另存为副本…/复制全文/创建便签/发送到 AI 对话；
 * - 次级动作只执行原子输出，**不改变主保存目标**。
 *
 * 纯函数（targetDisplay）与菜单清单可测；类只做 DOM 绑定与 IPC 编排。
 * 依赖全部注入（deps），测试用 fake 替换。
 */

import {iconHTML} from "../shared/icon.js";
import {saveDialog} from "../shared/tauri.js";
import {t} from "../i18n/index.js";
import {copyToClipboard, createStickyNote, runBuiltinAction} from "../shared/api.js";

/** "更多"菜单清单——顺序即展示顺序。 */
export const MENU_ITEMS = [
    {id: "save-to", icon: "folder", key: "editor.menu.saveTo"},
    {id: "save-copy", icon: "file-text", key: "editor.menu.saveCopy"},
    {id: "copy-all", icon: "copy", key: "editor.menu.copyAll"},
    {id: "create-sticky", icon: "sticky-note", key: "editor.menu.createSticky"},
    {id: "send-chat", icon: "send", key: "editor.menu.sendChat"},
];

/**
 * CommitTarget → 显示信息（纯函数）。
 * @param {{kind: string, path?: string, stickyId?: string}|null} target - 后端 CommitTarget（camelCase tagged）
 * @returns {{icon: string, key: string, args?: object}} i18n 渲染所需信息
 */
export function targetDisplay(target) {
    switch (target?.kind) {
        case "update_sticky":
            return {icon: "sticky-note", key: "editor.target.sticky"};
        case "confirmed_file":
            return {icon: "file-text", key: "editor.target.file", args: {path: target.path ?? ""}};
        case "return_to_caller":
            return {icon: "external-link", key: "editor.target.caller"};
        case "clipboard_result":
        default:
            return {icon: "copy", key: "editor.target.clipboard"};
    }
}

/**
 * 待发送文本：有选区时预填选区，无选区时预填全文（§3.8）。
 * @param {string} selection - 当前选区文本
 * @param {string} fullText - 全文
 */
export function pickSendText(selection, fullText) {
    const trimmed = (selection ?? "").trim();
    return trimmed.length > 0 ? selection : fullText;
}

export class EditorActions {
    /**
     * @param {object} deps
     * @param {*} deps.session - EditorSession 实例（isActive / target / commit）
     * @param {*} deps.adapter - EditorAdapter 实例（getSelectionText / getText）
     * @param {object} [deps.api] - IPC 依赖（测试注入 fake）
     * @param {(opts: object) => Promise<string|null>} [deps.api.saveDialog]
     * @param {(text: string) => Promise<*>} [deps.api.copyToClipboard]
     * @param {(content: string, color: string|null) => Promise<*>} [deps.api.createStickyNote]
     * @param {(id: string, arg: object|null) => Promise<*>} [deps.api.runBuiltinAction]
     * @param {object} callbacks
     * @param {(message: string) => void} [callbacks.onStatus] - 状态行文案（已翻译）
     * @param {(error: {code: string, message: string}) => void} [callbacks.onError] - 结构化保存错误
     */
    constructor({session, adapter, api = {}, callbacks = {}}) {
        this.session = session;
        this.adapter = adapter;
        this.api = {
            saveDialog,
            copyToClipboard,
            createStickyNote,
            runBuiltinAction,
            ...api,
        };
        this._callbacks = callbacks;
        this._menuEl = null;
        this._closeTimer = null;
    }

    /** 当前主保存目标（无会话时按剪贴板结果占位） */
    get target() {
        return this.session?.target ?? {kind: "clipboard_result"};
    }

    // ── 渲染 ────────────────────────────────────────────────────────────────

    /** 渲染主保存区真实去向（快照与提交结果后调用） */
    renderTarget(targetEl, iconUseEl) {
        if (!targetEl) return;
        const display = targetDisplay(this.target);
        const label = t(display.key, display.args);
        targetEl.textContent = label;
        targetEl.title = label;
        if (iconUseEl) {
            iconUseEl.setAttribute("href", `#icon-${display.icon}`);
        }
    }

    /** 构建"更多"菜单项（幂等重建） */
    renderMenu(menuEl) {
        if (!menuEl) return;
        this._menuEl = menuEl;
        menuEl.replaceChildren();
        for (const item of MENU_ITEMS) {
            const btn = document.createElement("button");
            btn.type = "button";
            btn.setAttribute("role", "menuitem");
            btn.dataset.action = item.id;
            btn.innerHTML = `${iconHTML(item.icon)}<span>${t(item.key)}</span>`;
            btn.addEventListener("click", () => {
                this.closeMenu();
                this.handleAction(item.id);
            });
            menuEl.appendChild(btn);
        }
    }

    toggleMenu() {
        if (!this._menuEl) return;
        this._menuEl.classList.contains("hidden") ? this.openMenu() : this.closeMenu();
    }

    openMenu() {
        this._menuEl?.classList.remove("hidden");
        return true;
    }

    closeMenu() {
        if (this._closeTimer) {
            clearTimeout(this._closeTimer);
            this._closeTimer = null;
        }
        this._menuEl?.classList.add("hidden");
    }

    /**
     * 悬浮缓冲关闭（§5.3）：mouseleave 启动 timer，mouseenter 取消。
     * 在 anchor 容器上绑定；bufferMs 供测试缩短。
     */
    bindMenuHover(anchorEl, {bufferMs = 250} = {}) {
        if (!anchorEl) return () => {};
        anchorEl.addEventListener("mouseleave", () => {
            if (this._menuEl?.classList.contains("hidden")) return;
            this._closeTimer = setTimeout(() => this.closeMenu(), bufferMs);
        });
        anchorEl.addEventListener("mouseenter", () => {
            if (this._closeTimer) {
                clearTimeout(this._closeTimer);
                this._closeTimer = null;
            }
        });
        // 点击菜单外关闭（点击 anchor 本身由 toggle 处理）
        const onDocClick = (e) => {
            if (anchorEl.contains(e.target)) return;
            this.closeMenu();
        };
        document.addEventListener("click", onDocClick);
        return () => document.removeEventListener("click", onDocClick);
    }

    // ── 动作分派 ────────────────────────────────────────────────────────────

    /** 菜单动作入口（主窗口测试/键盘可达性也可直接调用） */
    async handleAction(id) {
        switch (id) {
            case "save-to":
                return this.saveTo();
            case "save-copy":
                return this.saveCopy();
            case "copy-all":
                return this.copyAll();
            case "create-sticky":
                return this.createSticky();
            case "send-chat":
                return this.sendToChat();
            default:
                console.warn(`[editor-actions] 未知动作: ${id}`);
        }
    }

    /** 保存到…：选路径 → 提交并切换主目标（§3.5，失败不改变任何状态） */
    async saveTo() {
        if (!this.session?.isActive) return;
        const path = await this.api.saveDialog({
            title: t("editor.menu.saveTo"),
            filters: [{name: "Markdown / 文本", extensions: ["md", "txt"]}, {name: "所有文件", extensions: ["*"]}],
        });
        if (!path) return; // 用户取消：无副作用
        const result = await this.session.commit({kind: "save_to_file", path});
        this._reportCommit(result);
    }

    /** 另存为副本…：选路径 → 单次写副本，主目标不变（§3.5） */
    async saveCopy() {
        if (!this.session?.isActive) return;
        const path = await this.api.saveDialog({
            title: t("editor.menu.saveCopy"),
            filters: [{name: "Markdown / 文本", extensions: ["md", "txt"]}, {name: "所有文件", extensions: ["*"]}],
        });
        if (!path) return;
        const result = await this.session.commit({kind: "save_copy_to_file", path});
        // 副本成功只提示，不刷新主目标显示（目标未变）
        if (result?.ok) {
            this._callbacks.onStatus?.(t("editor.savedCopy", {path}));
        } else if (result?.error) {
            this._callbacks.onError?.(result.error);
        }
    }

    /** 复制全文（用户显式复制，进剪贴板历史；不改主目标） */
    async copyAll() {
        if (!this.session?.isActive) return;
        try {
            await this.api.copyToClipboard(this.adapter.getText());
            this._callbacks.onStatus?.(t("editor.copied"));
        } catch (e) {
            console.error("[editor-actions] 复制失败:", e);
            this._callbacks.onStatus?.(t("editor.actionFailed", {message: String(e)}));
        }
    }

    /** 创建便签（全文；不改主目标，不关闭编辑器） */
    async createSticky() {
        if (!this.session?.isActive) return;
        try {
            await this.api.createStickyNote(this.adapter.getText(), null);
            this._callbacks.onStatus?.(t("editor.stickyCreated"));
        } catch (e) {
            console.error("[editor-actions] 创建便签失败:", e);
            this._callbacks.onStatus?.(t("editor.actionFailed", {message: String(e)}));
        }
    }

    /** 发送到 AI 对话：选区优先，否则全文；只预填不自动发送（§3.8） */
    async sendToChat() {
        if (!this.session?.isActive) return;
        const text = pickSendText(this.adapter.getSelectionText(), this.adapter.getText());
        try {
            await this.api.runBuiltinAction("open_chat", {prefill: text});
            this._callbacks.onStatus?.(t("editor.sentToChat"));
        } catch (e) {
            console.error("[editor-actions] 发送到 AI 对话失败:", e);
            this._callbacks.onStatus?.(t("editor.actionFailed", {message: String(e)}));
        }
    }

    /** 保存类提交结果统一上报（saveTo：成功需刷新目标显示） */
    _reportCommit(result) {
        if (result?.ok) {
            this._callbacks.onStatus?.(t("editor.savedToFile"));
            this._callbacks.onTargetSaved?.(this.session.target);
            return;
        }
        if (result?.error) {
            this._callbacks.onError?.(result.error);
        }
    }
}
