/**
 * 清理确认 modal（0.22.5 H3，0.22.6 收敛）。
 *
 * 铁则：
 * - cleanup 按钮点击只打开 modal，绝不直接 invoke cleanup。
 * - modal 从最新 storage DTO 渲染后端返回的精确 targets。
 * - 每项显示 标签/size/current/shared/removable/blocked_reason/受影响引擎。
 * - target 文本只用 textContent，不信任或渲染前端拼造路径。
 * - current 和 blocked target 禁止选择。
 * - 默认不全选高风险项（非 shared 且 removable 才默认选）。
 * - confirm 只提交用户勾选的 target_id；后端重新解析 target_id。
 * - confirm 期间禁用重复提交。
 * - 成功后关闭 modal 并刷新 status/storage。
 * - 失败保留 modal 与选择，展示结构化错误。
 * - cancel/overlay/Escape 关闭不执行操作。
 * - 关闭后恢复触发按钮焦点。
 * - dispose/tab 切换时强制关闭 modal 并清除 pending targets。
 *
 * 公共缓存单独确认：
 * - 聚合所有 engine storage snapshot 中的 shared targets（后端 shared 标志）。
 * - 按 target_id 去重，显示 affected_engine_ids。
 * - 与单引擎分区。
 * - 公共缓存不进入普通"清理此引擎"的默认选择。
 * - 点击"清理公共缓存"打开独立风险确认状态。
 * - blocked/不可安全删除时禁用并展示原因。
 * - 不允许前端构造任意共享 target。
 *
 * modal 与键盘：
 * - hidden/aria-hidden/aria-modal 一致状态。
 * - 打开时聚焦标题/首个可选 target/取消按钮。
 * - Escape 先关闭 modal，不隐藏 settings 窗口。
 * - Tab 焦点不跑到 modal 后方（轻量 focus trap）。
 * - overlay 点击关闭但不 confirm。
 * - dispose 时移除本轮注册的 listeners，保证只注册一次。
 * - 中文 font-style: normal。
 * - 不新增 inline style.display。
 * - 所有图标用 Lucide。
 *
 * @module local-engine-cleanup-modal
 */

import {t} from "../../../i18n/index.js";

// ── 常量 ─────────────────────────────────────────────────────────────────────

/**
 * target 是否为公共/共享资产（需要单独确认）。
 * 只消费后端声明的 `shared` 标志——不按 kind 字符串复制后端分类规则。
 * @param {Object} target - StorageTargetDto
 * @returns {boolean}
 */
function isSharedTarget(target) {
    return !!target?.shared;
}

/**
 * target 是否可选择（removable 且无 blocked_reason 且非 current）。
 * @param {Object} target
 * @returns {boolean}
 */
function isSelectable(target) {
    return !!target.removable && !target.blocked_reason && !target.current;
}

/**
 * 格式化字节数为人类可读。
 * @param {number} bytes
 * @returns {string}
 */
function formatBytes(bytes) {
    if (!bytes || bytes === 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

// ── modal controller ──────────────────────────────────────────────────────────

/**
 * 创建 cleanup modal controller。
 *
 * @param {Object} opts
 * @param {HTMLElement} opts.modalEl - modal 根元素
 * @param {HTMLElement} opts.bodyEl - modal body（targets 列表容器）
 * @param {HTMLElement} opts.confirmBtn - 确认按钮
 * @param {Function} opts.onConfirm - 确认回调 `(targetIds, mode) => Promise<void>`
 * @param {Function} [opts.onClose] - 关闭回调
 * @returns {Object} modal controller
 */
export function createCleanupModal(opts) {
    const {modalEl, bodyEl, confirmBtn} = opts;
    let disposed = false;
    let listeners = [];
    let lastTriggerEl = null;
    let pendingTargets = [];
    let selectedIds = new Set();
    let mode = "engine"; // "engine" | "shared"
    let submitting = false;
    let keydownHandler = null;
    let focusTrapHandler = null;

    function tt(key, fallback) {
        const v = t(key);
        return v === key ? fallback : v;
    }

    /**
     * 打开 modal。
     * @param {Object} params
     * @param {HTMLElement} [params.triggerEl] - 触发按钮（关闭后恢复焦点）
     * @param {Array} params.targets - StorageTargetDto 列表
     * @param {"engine"|"shared"} params.mode
     */
    function open({triggerEl, targets, mode: m}) {
        if (disposed) return;
        close(false); // 先清理上一轮

        lastTriggerEl = triggerEl || null;
        pendingTargets = targets || [];
        selectedIds = new Set();
        mode = m || "engine";

        renderTargets();

        modalEl.hidden = false;
        modalEl.setAttribute("aria-hidden", "false");

        // 注册 listeners（保证只注册一次）
        registerListeners();

        // 聚焦：标题 → 首个可选 target → 取消按钮
        focusInitial();
    }

    /**
     * 关闭 modal。
     * @param {boolean} restoreFocus - 是否恢复触发按钮焦点
     */
    function close(restoreFocus = true) {
        if (disposed) return;

        modalEl.hidden = true;
        modalEl.setAttribute("aria-hidden", "true");

        unregisterListeners();

        pendingTargets = [];
        selectedIds = new Set();
        submitting = false;
        confirmBtn.disabled = false;

        // 清空 body
        bodyEl.textContent = "";

        if (restoreFocus && lastTriggerEl && typeof lastTriggerEl.focus === "function") {
            lastTriggerEl.focus({preventScroll: true});
        }
        lastTriggerEl = null;

        if (opts.onClose) opts.onClose();
    }

    /**
     * 渲染 targets 列表。
     */
    function renderTargets() {
        bodyEl.textContent = "";

        if (pendingTargets.length === 0) {
            const empty = document.createElement("p");
            empty.className = "le-cleanup-empty";
            empty.textContent = tt("local_engine.cleanup.no_targets", "无可清理的目标");
            bodyEl.appendChild(empty);
            confirmBtn.disabled = true;
            return;
        }

        // 模式标题
        const desc = document.createElement("p");
        desc.className = "le-cleanup-desc";
        if (mode === "shared") {
            desc.textContent = tt("local_engine.cleanup.shared_desc", "公共缓存被多个引擎共享，清理后可能需要重新下载。");
        } else {
            desc.textContent = tt("local_engine.cleanup.engine_desc", "选择要清理的目标。当前环境与被引用的共享资产不会被清理。");
        }
        bodyEl.appendChild(desc);

        const list = document.createElement("div");
        list.className = "le-cleanup-list";
        list.setAttribute("role", "group");

        for (const target of pendingTargets) {
            list.appendChild(renderTargetItem(target));
        }

        bodyEl.appendChild(list);

        // 更新 confirm 按钮状态
        updateConfirmState();
    }

    /**
     * 渲染单个 target 项。
     */
    function renderTargetItem(target) {
        const item = document.createElement("div");
        item.className = "le-cleanup-item";
        item.dataset.targetId = target.target_id;

        const selectable = isSelectable(target);
        const isShared = isSharedTarget(target);
        const blocked = !!target.blocked_reason || target.current;

        // checkbox
        const checkbox = document.createElement("input");
        checkbox.type = "checkbox";
        checkbox.className = "le-cleanup-checkbox";
        checkbox.dataset.targetId = target.target_id;
        checkbox.disabled = !selectable;
        checkbox.checked = selectable && !isShared && target.removable && !target.current;

        // 默认不全选高风险项：shared 不默认选
        if (isShared) {
            checkbox.checked = false;
        }

        checkbox.addEventListener("change", () => {
            if (checkbox.checked) {
                selectedIds.add(target.target_id);
            } else {
                selectedIds.delete(target.target_id);
            }
            updateConfirmState();
        });

        if (selectable && checkbox.checked) {
            selectedIds.add(target.target_id);
        }

        const label = document.createElement("label");
        label.className = "le-cleanup-item-label";
        label.appendChild(checkbox);

        // 主信息
        const info = document.createElement("div");
        info.className = "le-cleanup-item-info";

        // 标签（textContent）
        const labelText = target.label_fallback || target.target_id;
        const labelSpan = document.createElement("span");
        labelSpan.className = "le-cleanup-item-title";
        labelSpan.textContent = labelText;
        info.appendChild(labelSpan);

        // 安装路径（0.22.9：后端填充 path_display——回答"这些东西装在哪"）
        if (target.path_display) {
            const path = document.createElement("div");
            path.className = "le-cleanup-item-path";
            path.textContent = target.path_display;
            info.appendChild(path);
        }

        // scope/size/flags/affected 一行 badge（样式 .le-cleanup-item-meta）
        const meta = document.createElement("div");
        meta.className = "le-cleanup-item-meta";

        // scope/kind
        const scope = document.createElement("span");
        scope.className = "le-cleanup-item-scope";
        scope.textContent = tt(`local_engine.cleanup.scope.${target.kind}`, target.kind);
        meta.appendChild(scope);

        // size
        const size = document.createElement("span");
        size.className = "le-cleanup-item-size";
        size.textContent = formatBytes(target.size_bytes);
        meta.appendChild(size);

        // flags: current/shared/removable
        const flags = document.createElement("span");
        flags.className = "le-cleanup-item-flags";
        const flagParts = [];
        if (target.current) flagParts.push(tt("local_engine.cleanup.flag.current", "当前"));
        if (target.shared) flagParts.push(tt("local_engine.cleanup.flag.shared", "共享"));
        if (target.removable) flagParts.push(tt("local_engine.cleanup.flag.removable", "可清理"));
        flags.textContent = flagParts.join(" · ");
        meta.appendChild(flags);

        // 受影响引擎
        if (target.affected_engine_ids && target.affected_engine_ids.length > 0) {
            const affected = document.createElement("span");
            affected.className = "le-cleanup-item-affected";
            affected.textContent = `${tt("local_engine.cleanup.affected_engines", "影响引擎")}: ${target.affected_engine_ids.join(", ")}`;
            meta.appendChild(affected);
        }

        // blocked_reason（警示色独立于 badge 行）
        if (target.blocked_reason) {
            const reason = document.createElement("span");
            reason.className = "le-cleanup-item-blocked";
            reason.textContent = `${tt("local_engine.cleanup.blocked", "不可清理")}: ${target.blocked_reason}`;
            meta.appendChild(reason);
        }

        info.appendChild(meta);

        label.appendChild(info);
        item.appendChild(label);
        return item;
    }

    /**
     * 更新 confirm 按钮状态。
     */
    function updateConfirmState() {
        if (submitting) return;
        confirmBtn.disabled = selectedIds.size === 0;
    }

    /**
     * 注册 modal listeners（保证只注册一次）。
     */
    function registerListeners() {
        unregisterListeners();

        // overlay / cancel / close 按钮
        const closeEls = modalEl.querySelectorAll("[data-le-cleanup-close]");
        for (const el of closeEls) {
            const handler = (e) => {
                e.preventDefault();
                e.stopPropagation();
                close(true);
            };
            el.addEventListener("click", handler);
            listeners.push({el, type: "click", fn: handler});
        }

        // confirm 按钮
        const confirmHandler = () => {
            if (submitting) return;
            handleConfirm();
        };
        confirmBtn.addEventListener("click", confirmHandler);
        listeners.push({el: confirmBtn, type: "click", fn: confirmHandler});

        // Escape：modal 打开时先关闭 modal，不隐藏 settings 窗口
        keydownHandler = (e) => {
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                close(true);
            } else if (e.key === "Tab") {
                // 轻量 focus trap
                trapFocus(e);
            }
        };
        modalEl.addEventListener("keydown", keydownHandler);
        listeners.push({el: modalEl, type: "keydown", fn: keydownHandler});
    }

    /**
     * 轻量 focus trap——Tab 不跑到 modal 后方。
     */
    function trapFocus(e) {
        const focusable = modalEl.querySelectorAll(
            'input:not([disabled]), button:not([disabled]), [tabindex]:not([tabindex="-1"])',
        );
        if (focusable.length === 0) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (e.shiftKey) {
            if (document.activeElement === first) {
                e.preventDefault();
                last.focus();
            }
        } else {
            if (document.activeElement === last) {
                e.preventDefault();
                first.focus();
            }
        }
    }

    /**
     * 卸载 listeners。
     */
    function unregisterListeners() {
        for (const {el, type, fn} of listeners) {
            try {
                el.removeEventListener(type, fn);
            } catch (e) {
                // ignore
            }
        }
        listeners = [];
        keydownHandler = null;
    }

    /**
     * 初始聚焦。
     */
    function focusInitial() {
        // 优先：首个可选 checkbox
        const firstSelectable = modalEl.querySelector('.le-cleanup-checkbox:not(:disabled)');
        if (firstSelectable) {
            firstSelectable.focus();
            return;
        }
        // 其次：取消按钮
        const cancelBtn = modalEl.querySelector("[data-le-cleanup-close]");
        if (cancelBtn) {
            cancelBtn.focus();
            return;
        }
        // 最后：标题
        const title = modalEl.querySelector(".le-cleanup-title");
        if (title && typeof title.focus === "function") {
            title.focus();
        } else if (typeof modalEl.focus === "function") {
            modalEl.focus();
        }
    }

    /**
     * 处理确认。
     */
    async function handleConfirm() {
        if (submitting) return;
        if (selectedIds.size === 0) return;

        submitting = true;
        confirmBtn.disabled = true;

        // 展示 submitting 状态
        const originalText = confirmBtn.textContent;
        confirmBtn.textContent = tt("local_engine.cleanup.confirming", "清理中…");

        try {
            const targetIds = Array.from(selectedIds);
            if (opts.onConfirm) {
                await opts.onConfirm(targetIds, mode);
            }
            // 成功 → 关闭 modal（不恢复触发按钮焦点，因为通常卡片会重渲染）
            close(false);
        } catch (e) {
            // 失败 → 保留 modal 与选择，展示结构化错误
            renderError(e);
        } finally {
            submitting = false;
            confirmBtn.disabled = false;
            confirmBtn.textContent = originalText;
            updateConfirmState();
        }
    }

    /**
     * 渲染结构化错误。
     */
    function renderError(err) {
        // 移除旧错误
        const oldError = bodyEl.querySelector(".le-cleanup-error");
        if (oldError) oldError.remove();

        const errorDiv = document.createElement("div");
        errorDiv.className = "le-cleanup-error";

        const msg = err?.message || err?.detail || String(err);
        const main = document.createElement("p");
        main.className = "le-cleanup-error-main";
        main.textContent = msg;
        errorDiv.appendChild(main);

        // 如果有 action_hint
        if (err && err.action_hint && err.action_hint !== msg) {
            const hint = document.createElement("p");
            hint.className = "le-cleanup-error-hint";
            hint.textContent = err.action_hint;
            errorDiv.appendChild(hint);
        }

        bodyEl.appendChild(errorDiv);
    }

    /**
     * dispose——强制关闭并清除 pending targets。
     */
    function dispose() {
        disposed = true;
        unregisterListeners();
        pendingTargets = [];
        selectedIds = new Set();
        modalEl.hidden = true;
        modalEl.setAttribute("aria-hidden", "true");
        bodyEl.textContent = "";
    }

    return {
        open,
        close,
        dispose,
        isOpen() {
            return !modalEl.hidden;
        },
    };
}

// ── 公共缓存聚合 ──────────────────────────────────────────────────────────────

/**
 * 从多个引擎的 storage snapshot 中聚合公共/共享 targets。
 *
 * - 只返回后端标记 `shared` 的 target（不按 kind 复制后端分类规则）。
 * - 按 target_id 去重，合并 affected_engine_ids。
 *
 * @param {Map<string, Object>} state - engine_id → EngineStateEntry
 * @returns {Array} 聚合后的 shared targets
 */
export function aggregateSharedTargets(state) {
    const merged = new Map(); // target_id → target

    for (const [engineId, entry] of state) {
        const storage = entry.storage;
        if (!storage || !storage.targets) continue;

        for (const target of storage.targets) {
            if (!isSharedTarget(target)) continue;

            const existing = merged.get(target.target_id);
            if (existing) {
                // 合并 affected_engine_ids
                const affected = new Set(existing.affected_engine_ids || []);
                if (existing.engine_id) affected.add(existing.engine_id);
                if (target.engine_id) affected.add(target.engine_id);
                merged.set(target.target_id, {
                    ...existing,
                    affected_engine_ids: Array.from(affected),
                });
            } else {
                const affected = new Set();
                if (target.engine_id) affected.add(target.engine_id);
                merged.set(target.target_id, {
                    ...target,
                    affected_engine_ids: Array.from(affected),
                });
            }
        }
    }

    return Array.from(merged.values());
}
