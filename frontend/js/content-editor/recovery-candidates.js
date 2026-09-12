/**
 * 恢复候选编排（0.23.6 恢复链路收口）。
 *
 * 两个职责：
 * 1. `RecoveryConflictResolver` —— bind 时探测到的同键草稿与当前正文冲突时
 *    的四动作决策（keep / restore / copy / dismiss）。核心铁则：
 *    - 关闭弹窗、Esc、遮罩、异常 = "暂不处理"：不写剪贴板、不清理候选、
 *      不动当前正文；
 *    - "复制草稿"必须是显式点击，复制失败保留候选；
 *    - 任何破坏性动作前重新校验完整围栏（session/generation/source
 *      instance/adapter revision/正文），迟到响应不得覆盖新输入；
 *    - "保留当前 / 复制候选"只按冻结版本墙清理旧候选（discardCandidate），
 *      绝不借执行时刻的最新草稿身份，随后立即 flush 当前 dirty 正文。
 *
 * 2. `RecoveryCandidates` —— "更多 → 恢复未保存草稿…"列表入口：临时来源
 *    草稿每次打开获得新键，崩溃后唯一可靠的找回方式就是这里的人工列表。
 *    每个候选展示更新时间/来源类型/正文长度/orphan 标记与谨慎预览；
 *    恢复/复制/删除各自携带候选自身的完整版本墙，互不串稿。
 *
 * 依赖全部注入（ipc / 状态读取 / 弹窗 / 文案），保持模块可独立测试。
 */

import {recoveryFenceMatches} from "./recovery-draft.js";

/**
 * 恢复冲突四动作解析器。
 *
 * @param {object} deps
 * @param {{copyToClipboard: (text: string) => Promise<*>}} deps.api
 * @param {*} deps.draft - RecoveryDraft 实例（discardCandidate / flushVerified）
 * @param {() => object|null} deps.getFenceState - 当前完整围栏快照
 * @param {(text: string) => boolean} deps.restoreIntoEditor - 替换工作正文（含 UI 刷新）
 * @param {(opts: object) => Promise<"keep"|"restore"|"copy"|"dismiss">} deps.showDialog
 * @param {(message: string) => void} [deps.onStatus]
 * @param {(key: string, params?: object) => string} [deps.t]
 */
export class RecoveryConflictResolver {
    constructor({api, draft, getFenceState, restoreIntoEditor, showDialog, onStatus, t}) {
        this._api = api;
        this._draft = draft;
        this._getFenceState = getFenceState;
        this._restoreIntoEditor = restoreIntoEditor;
        this._showDialog = showDialog;
        this._onStatus = onStatus ?? (() => {});
        this._t = t ?? ((key, params) => key);
    }

    /**
     * 解析一次恢复冲突。`frozen` 是磁盘读取前冻结的围栏，`candidate` 是
     * `RecoveryDraft.restore()` 的返回值（含 identity 版本墙）。
     * @returns {Promise<{action: string}>}
     */
    async resolve(frozen, candidate) {
        const choice = await this._showDialog({
            untrusted: candidate.untrusted === true,
        });
        // "暂不处理"（含 Esc/遮罩/异常 fallback）：一切保持原样。
        if (choice === "dismiss") {
            this._onStatus(this._t("editor.draft.deferred"));
            return {action: "dismiss"};
        }
        // 任何会改正文/清理候选的动作前先重校验围栏；不匹配即迟到响应。
        if (!recoveryFenceMatches(frozen, this._getFenceState())) {
            this._onStatus(this._t("editor.draft.staleFence"));
            return {action: "stale"};
        }
        if (choice === "restore") {
            if (!this._restoreIntoEditor(candidate.text)) {
                this._onStatus(this._t("editor.draft.restoreFailed"));
                return {action: "restore-failed"};
            }
            return {action: "restored"};
        }
        if (choice === "copy") {
            try {
                await this._api.copyToClipboard(candidate.text);
            } catch (e) {
                // 复制失败：候选保留、当前正文不动，明确报错。
                console.error("[recovery] 候选复制失败:", e);
                this._onStatus(this._t("editor.draft.copyFailed"));
                return {action: "copy-failed"};
            }
            const persisted = await this._persistCurrentBeforeCleanup(frozen, candidate.identity);
            if (!persisted.ok) return {action: persisted.action};
            const cleared = await this._draft.discardCandidate(candidate.identity);
            if (!cleared.cleared && !persisted.sameKeySuperseded) {
                this._onStatus(this._t("editor.draft.deleteFailed"));
                return {action: "copy-cleanup-failed"};
            }
            this._onStatus(this._t("editor.draft.copied"));
            return {action: "copied"};
        }
        // "keep"（保留当前）：必须先可靠落盘当前正文，再清理旧候选。
        // 反过来会在写盘失败时先删除唯一可恢复副本。
        const persisted = await this._persistCurrentBeforeCleanup(frozen, candidate.identity);
        if (!persisted.ok) return {action: persisted.action};
        const cleared = await this._draft.discardCandidate(candidate.identity);
        if (!cleared.cleared && !persisted.sameKeySuperseded) {
            this._onStatus(this._t("editor.draft.deleteFailed"));
            return {action: "keep-cleanup-failed"};
        }
        this._onStatus(this._t("editor.draft.kept"));
        return {action: "kept"};
    }

    async _persistCurrentBeforeCleanup(frozen, candidateIdentity) {
        const verified = await this._draft.flushVerified();
        if (!verified.ok) {
            this._onStatus(this._t("editor.draft.flushFailed"));
            return {ok: false, action: "flush-failed"};
        }
        if (!recoveryFenceMatches(frozen, this._getFenceState())) {
            this._onStatus(this._t("editor.draft.staleFence"));
            return {ok: false, action: "stale"};
        }
        return {
            ok: true,
            // 同键当前正文写入会自然替换旧候选；此时旧墙 clear 返回 false
            // 是安全的版本墙拒绝，而不是清理失败。
            sameKeySuperseded: candidateIdentity?.key === frozen?.key
                && (verified.saved === true || verified.alreadyPersisted === true),
        };
    }
}

/** 候选键 → 来源类型（键前缀是后端契约：sticky 稳定键 / editor: 临时实例键）。 */
export function candidateSourceKind(key) {
    return typeof key === "string" && key.startsWith("sticky:") ? "sticky" : "temporary";
}

/**
 * 谨慎预览：压缩空白后取首行，截断到 maxLen 字符。空正文返回空串
 * （长度信息仍由 meta 行表达）。
 */
export function candidatePreview(body, maxLen = 60) {
    if (typeof body !== "string" || body.length === 0) return "";
    const firstLine = body.split(/\r?\n/, 1)[0].replace(/\s+/g, " ").trim();
    if (firstLine.length <= maxLen) return firstLine;
    return `${firstLine.slice(0, maxLen)}…`;
}

/**
 * 恢复候选列表控制器（"更多 → 恢复未保存草稿…"）。
 *
 * @param {object} deps
 * @param {{listEditorDrafts: () => Promise<Array>, clearEditorDraft: (req: object) => Promise<*>,
 *          copyToClipboard: (text: string) => Promise<*>}} deps.api
 * @param {() => object|null} deps.getFenceState - 当前围栏（sessionActive 标记无会话）
 * @param {() => boolean} deps.isDirty
 * @param {() => Promise<{ok: boolean}>} deps.flushVerified - 验证式落盘当前正文
 * @param {(text: string) => boolean} deps.restoreIntoEditor
 * @param {(message: string, opts?: object) => Promise<boolean>} deps.confirmDialog - 危险动作确认
 * @param {(ms: number) => string} [deps.formatTime]
 * @param {(message: string) => void} [deps.onStatus]
 * @param {(key: string, params?: object) => string} [deps.t]
 */
export class RecoveryCandidates {
    constructor({
        api,
        getFenceState,
        isDirty,
        flushVerified,
        restoreIntoEditor,
        confirmDialog,
        formatTime,
        onStatus,
        t,
    }) {
        this._api = api;
        this._getFenceState = getFenceState;
        this._isDirty = isDirty;
        this._flushVerified = flushVerified;
        this._restoreIntoEditor = restoreIntoEditor;
        this._confirmDialog = confirmDialog;
        this._formatTime = formatTime ?? ((ms) => new Date(ms).toLocaleString());
        this._onStatus = onStatus ?? (() => {});
        this._t = t ?? ((key, params) => key);
        this._open = false;
        this._overlay = null;
    }

    /** 是否有列表在展示（测试/防重入） */
    get isOpen() {
        return this._open;
    }

    /** 打开候选列表。无候选时仅提示，不弹层。 */
    async open() {
        if (this._open) return;
        this._open = true; // 入口即置位：读取期间的重复点击不叠层
        let drafts;
        try {
            drafts = await this._api.listEditorDrafts();
        } catch (e) {
            this._open = false;
            console.error("[recovery] 候选列表读取失败:", e);
            this._onStatus(this._t("editor.draft.list.failed"));
            return;
        }
        const frozen = this._getFenceState();
        const activeKey = frozen?.sessionActive ? frozen.key : null;
        const candidates = (Array.isArray(drafts) ? drafts : [])
            .filter((d) => d && typeof d.key === "string" && d.key.length > 0)
            .filter((d) => d.key !== activeKey);
        if (candidates.length === 0) {
            this._open = false;
            this._onStatus(this._t("editor.draft.list.empty"));
            return;
        }
        this._render(candidates, frozen);
    }

    /** 渲染列表层（DOM 结构复用 modal/confirm-dialog 样式） */
    _render(candidates, frozen) {
        const overlay = document.createElement("div");
        overlay.className = "modal-overlay confirm-dialog-overlay recovery-list-overlay";

        const card = document.createElement("div");
        card.className = "confirm-dialog recovery-list-dialog";
        card.setAttribute("role", "dialog");
        card.setAttribute("aria-label", this._t("editor.draft.list.title"));

        const titleEl = document.createElement("div");
        titleEl.className = "confirm-dialog-title recovery-list-title";
        titleEl.textContent = this._t("editor.draft.list.title");

        const listEl = document.createElement("div");
        listEl.className = "recovery-list";
        listEl.setAttribute("role", "list");

        for (const candidate of candidates) {
            listEl.appendChild(this._renderRow(candidate, frozen));
        }

        const actionsEl = document.createElement("div");
        actionsEl.className = "confirm-dialog-actions";
        const closeBtn = document.createElement("button");
        closeBtn.type = "button";
        closeBtn.className = "btn-primary";
        closeBtn.textContent = this._t("editor.draft.close");
        closeBtn.addEventListener("click", () => this._close());
        actionsEl.appendChild(closeBtn);

        card.appendChild(titleEl);
        card.appendChild(listEl);
        card.appendChild(actionsEl);
        overlay.appendChild(card);

        // 遮罩点击 = 关闭（无副作用；候选与当前正文都保持原样）
        overlay.addEventListener("click", (e) => {
            if (e.target === overlay) this._close();
        });
        const onKey = (e) => {
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                this._close();
            }
        };
        document.addEventListener("keydown", onKey, true);
        this._keydownHandler = onKey;
        this._overlay = overlay;
        document.body.appendChild(overlay);
    }

    _renderRow(candidate, frozen) {
        const identity = {
            key: candidate.key,
            sessionRef: String(candidate.sessionRef ?? ""),
            generation: Number(candidate.generation ?? 0),
            revision: Number(candidate.revision ?? 0),
            hash: String(candidate.hash ?? ""),
        };
        const row = document.createElement("div");
        row.className = "recovery-item";
        row.setAttribute("role", "listitem");

        const metaEl = document.createElement("div");
        metaEl.className = "recovery-item-meta";
        const kindKey = candidateSourceKind(candidate.key) === "sticky"
            ? "editor.draft.kind.sticky"
            : "editor.draft.kind.temporary";
        const bodyLen = typeof candidate.body === "string" ? candidate.body.length : 0;
        metaEl.textContent = this._t("editor.draft.list.meta", {
            time: this._formatTime(Number(candidate.updatedAtMs ?? 0)),
            kind: this._t(kindKey),
            count: bodyLen,
        });
        if (candidate.orphaned === true) {
            const badge = document.createElement("span");
            badge.className = "recovery-item-badge";
            badge.textContent = this._t("editor.draft.orphanBadge");
            metaEl.appendChild(badge);
        }

        const previewEl = document.createElement("div");
        previewEl.className = "recovery-item-preview";
        previewEl.textContent = candidatePreview(candidate.body);

        const opsEl = document.createElement("div");
        opsEl.className = "recovery-item-ops";

        const restoreBtn = document.createElement("button");
        restoreBtn.type = "button";
        restoreBtn.className = "btn btn-small";
        restoreBtn.textContent = this._t("editor.draft.restoreTo");
        if (!frozen?.sessionActive) restoreBtn.disabled = true;
        restoreBtn.addEventListener("click", () => {
            void this._restore(candidate, frozen);
        });
        opsEl.appendChild(restoreBtn);

        const copyBtn = document.createElement("button");
        copyBtn.type = "button";
        copyBtn.className = "btn btn-small";
        copyBtn.textContent = this._t("editor.draft.copyBtn");
        copyBtn.addEventListener("click", () => {
            void this._copy(candidate);
        });
        opsEl.appendChild(copyBtn);

        const deleteBtn = document.createElement("button");
        deleteBtn.type = "button";
        deleteBtn.className = "btn btn-small";
        deleteBtn.textContent = this._t("editor.draft.deleteBtn");
        deleteBtn.addEventListener("click", () => {
            void this._delete(identity, row);
        });
        opsEl.appendChild(deleteBtn);

        row.appendChild(metaEl);
        row.appendChild(previewEl);
        row.appendChild(opsEl);
        return row;
    }

    /**
     * 恢复候选到当前编辑器。围栏重校验 + 当前 dirty 正文验证式落盘 +
     * （dirty 时）显式确认替换，任何一步失败都不动当前正文。
     */
    async _restore(candidate, frozen) {
        if (!recoveryFenceMatches(frozen, this._getFenceState())) {
            this._onStatus(this._t("editor.draft.staleFence"));
            this._close();
            return;
        }
        if (this._isDirty()) {
            const verified = await this._flushVerified();
            if (!verified.ok) {
                // 当前正文未确认落盘：不得被候选覆盖。
                this._onStatus(this._t("editor.draft.flushFailed"));
                return;
            }
            const confirmed = await this._confirmDialog(
                this._t("editor.draft.replaceWarning"),
                {kind: "warning", okLabel: this._t("editor.draft.restoreTo")},
            );
            if (!confirmed) return;
            // 确认弹窗等待期间可能又有输入/会话变化：再次校验围栏。
            if (!recoveryFenceMatches(frozen, this._getFenceState())) {
                this._onStatus(this._t("editor.draft.staleFence"));
                this._close();
                return;
            }
        }
        if (this._restoreIntoEditor(typeof candidate.body === "string" ? candidate.body : "")) {
            this._onStatus(this._t("editor.draft.restored"));
            this._close();
        } else {
            this._onStatus(this._t("editor.draft.restoreFailed"));
        }
    }

    /** 复制候选正文（显式点击；失败保留候选并报错）。 */
    async _copy(candidate) {
        try {
            await this._api.copyToClipboard(typeof candidate.body === "string" ? candidate.body : "");
            this._onStatus(this._t("editor.draft.copied"));
        } catch (e) {
            console.error("[recovery] 候选复制失败:", e);
            this._onStatus(this._t("editor.draft.copyFailed"));
        }
    }

    /** 删除候选：显式确认 + 候选自身完整版本墙；失败保留行并报错。 */
    async _delete(identity, row) {
        // 墙不完整（缺 hash/revision）时后端会静默拒绝；UI 不得假装已删除。
        if (!identity.hash || !Number.isFinite(identity.revision) || identity.revision < 0) {
            this._onStatus(this._t("editor.draft.deleteFailed"));
            return;
        }
        const confirmed = await this._confirmDialog(
            this._t("editor.draft.deleteWarning"),
            {kind: "warning", okLabel: this._t("editor.draft.deleteBtn")},
        );
        if (!confirmed) return;
        try {
            const cleared = await this._api.clearEditorDraft({
                key: identity.key,
                sessionRef: identity.sessionRef,
                generation: identity.generation,
                expectedRevision: identity.revision,
                expectedHash: identity.hash,
            });
            if (cleared !== true) {
                this._onStatus(this._t("editor.draft.deleteFailed"));
                return;
            }
        } catch (e) {
            console.error("[recovery] 候选删除失败:", e);
            this._onStatus(this._t("editor.draft.deleteFailed"));
            return;
        }
        row.remove();
        this._onStatus(this._t("editor.draft.deleted"));
        if (this._overlay && this._overlay.querySelectorAll(".recovery-item").length === 0) {
            this._close();
        }
    }

    _close() {
        if (!this._open) return;
        this._open = false;
        if (this._keydownHandler) {
            document.removeEventListener("keydown", this._keydownHandler, true);
            this._keydownHandler = null;
        }
        if (this._overlay) {
            const overlay = this._overlay;
            overlay.classList.add("confirm-dialog-closing");
            setTimeout(() => overlay.remove(), 150);
            this._overlay = null;
        }
    }
}
