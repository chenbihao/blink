/**
 * chat 窗口入口（0.12.1 Phase 5）。
 *
 * 装配 state / ipc / renderer / components / composer，注册事件监听。
 */

import * as state from "./state.js";
import * as ipc from "./ipc.js";
import {bindLinkOpener, initRenderer} from "./renderer.js";
import * as components from "./components.js";
import {forceScrollToBottom} from "./components.js";
import {escapeAttr, escapeText} from "./utils.js";
// 0.12.7 §6.3：显式导入 renderSignal，多处场景接入
import {
    addAttachments,
    clearAttachments,
    clearInput,
    clearSentAttachments,
    composerInputRevision,
    focusInput,
    hideVoiceIndicator,
    initComposer,
    insertAtCursor,
    isVoiceRecording,
    setInputMode,
    setInputValue,
    setStreamingMode,
    showVoiceError,
    showVoiceIndicator,
    showVoiceStatus,
    updateVoiceLevel,
    updateVoicePartial
} from "./composer.js";
import {initSidebar, refreshSidebar, setActiveConversation, toggleSidebar} from "./sidebar.js";
// 0.21.23：思考控件与模型选择器（含 flyout 定位）从本文件下沉为独立模块——
// 软规则：main.js 不再吸收新的交互模块，只保留装配与全局事件分发
import {effectiveThinkingEnabled, initThinkingSelector, syncThinkingControl} from "./thinking-selector.js";
import {bindModelSelector, currentModelSupportsVision, openModelDropdown, refreshModelSelector, updateProviderLabel} from "./model-selector.js";
import {wasConsumedByKeynav} from "./flyout-keynav.js";
import {initCommandPalette, invalidatePromptCache, invalidateSlashSnapshotCache, isPaletteOpen, closePalette, openPromptPalette} from "./command-palette.js";
import {initAttachmentPalette, splitAtQuery} from "./attachment-palette.js";
import {applyThemeFromConfig} from "../shared/theme.js";
import {getCurrentWindow, invoke, listen} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {promoteEphemeralConversation} from "../shared/api.js";
import {attachmentSummaryText} from "./outgoing.js";
import {initComposerBarPopup, invalidateComposerBarCache, refreshPopupIfVisible} from "./composer-bar-popup.js";
// invalidateComposerBarCache 仍在 handleContextStatus 中使用
// 0.12.4 §6.5：openSettings 直接用 invoke，不再需要动态 import
import {t} from "../i18n/index.js";
import {createEarlyStreamBuffer} from "../shared/early-stream-buffer.js";
import {createPrefillConsumer} from "./prefill.js";
import {createTurnRetryStore, resolvePersistedTurn} from "./turn-retry.js";
import {ensureSpriteLoaded} from "../shared/icon.js";

/** 流式渲染节流：requestAnimationFrame 句柄 */
let rafHandle = 0;

/** 当前 assistant 消息 DOM 引用 */
let currentAssistantEl = null;
const earlyStreamBuffer = createEarlyStreamBuffer();
const turnRetries = createTurnRetryStore();
let editingTurn = false;
const lastUserId = () => state.messages.findLast(msg => msg.role === "user")?.id;

/** 对话窗口是否已被用户实际打开（区分预热 vs 真正显示）。
 *  preheat 在启动 3s 后创建隐藏的 chat 窗口，JS init() 会执行但窗口不可见。
 *  此标记用于阻止预热阶段触发 MCP 连接——只在窗口首次获得焦点（真正显示）后才连接。 */
let windowActivated = false;

/** 当前 Tool 状态卡 DOM 引用 */
let currentToolEl = null;

// ── 初始化 ──────────────────────────────────────

async function init() {
    // chat 窗口也消费 Lucide sprite（如供应商设置的 cable 图标）。
    // 非阻塞加载，避免次级窗口首次初始化时延后事件监听器注册。
    ensureSpriteLoaded();
    // 主题初始化 + 实时跟随
    applyThemeFromConfig();
    // 0.12.2 §4.9: ai_config 变更时刷新模型选择器（复用 config-changed 事件）
    listen(EVENTS.CONFIG_CHANGED, (e) => {
        applyThemeFromConfig();
        if (e?.payload?.key === "ai_config") {
            invalidateSlashSnapshotCache();
            refreshModelSelector();
            refreshToolPool(true);
            invalidateComposerBarCache();
            refreshPopupIfVisible();
        }
        // 0.13.0：MCP 配置变更时刷新 tool 池
        if (e?.payload?.key === "mcp:servers") {
            // 配置 command 已负责 generation 失效与按需后台连接；这里仅刷新只读快照。
            invalidateSlashSnapshotCache();
            if (!windowActivated) return;
            refreshToolPool(true);
            invalidateComposerBarCache();
            refreshPopupIfVisible();
        }
    });

    initRenderer();
// 拦截对话消息内的链接点击，通过外部浏览器打开（防止 Tauri WebView 内部导航崩溃）
    bindLinkOpener();
    components.initComponents({onEditMessage: handleEditMessage});

    initComposer({
        onSend: handleSend,
        onStop: handleStop,
        // 0.23.12：composer 附件 picker 需要当前会话 id 做竞态校验
        // （picker 返回时会话已切换则撤销刚签发的 ref）
        getConversationId: () => state.conversationId,
    });
    // 0.21.17：思考控件初始视觉按默认态渲染；refreshModelSelector 拉到模型能力后
    // 再同步真实状态（支持等级 → 强度下拉，否则 → 简单开关）。
    syncThinkingControl();

    // 0.19：chat prefill 投递（revision 机制防残留/防旧事件误删）
    //
    // 两条路径都不丢文本：
    //   冷启动：listener 注册后 take 读取 pending，成功后 ack
    //   热窗口：listener 收到事件 → ack 清空 pending
    // revision 防止旧事件的 ack 误删新 pending。
    // 共享在途 Promise 防止 take 和 event 同时命中时重复填充。

    /**
     * 0.27.9：正文+附件草稿事务（§5.9.5/§8.2）——
     * newConversation 先建新会话；正文追加当前草稿（不覆盖、不自动发送）；
     * 附件进 composer 并 adopt 转移资源归属；资源转移失败不 ack
     * （pending 留 TTL 兜底，前端不误删草稿）。
     * @returns {Promise<boolean>} 是否应 ack 该 revision
     */
    const applyPrefillPayload = createPrefillConsumer({
        getConversationId: () => state.conversationId,
        newConversation: handleNewConversation,
        insertText: text => { insertAtCursor(text); focusInput(); },
        adopt: ipc.adoptChatAttachments,
        add: addAttachments,
    });

    // 先 await listen，确认 listener 注册成功后再 take
    await listen(EVENTS.CHAT_PREFILL, (event) => {
        const payload = event?.payload || {};
        const revision = payload?.revision || 0;
        applyPrefillPayload(payload, revision).then((shouldAck) => {
            // 热窗口路径：转移失败不 ack（§5.9.5），成功/空载荷 ack 清 pending
            if (revision && shouldAck) {
                ipc.ackChatPrefill(revision).catch(() => {
                });
            }
        });
    });

    // listener 已注册，现在 take 兜底冷启动路径
    // 两种顺序都不会丢：
    //   pending 先写入 → take 拉取 → event 后到同 revision 被 appliedRevision 去重
    //   take 返回 null → event 后到正常填充 + ack
    try {
        const pending = await ipc.takeChatPrefill();
        if (pending && (pending.text || (pending.attachments || []).length > 0)) {
            const shouldAck = await applyPrefillPayload(pending, pending.revision);
            if (pending.revision && shouldAck) {
                ipc.ackChatPrefill(pending.revision).catch(() => {
                });
            }
        }
    } catch (e) {
        console.warn("[chat] take_chat_prefill 失败:", e);
    }

    initSidebar({
        onSwitch: handleSwitchConversation,
        onNew: handleNewConversation,
        onRenamed: handleSidebarRenamed,
        onExport: handleExportConversation,
    });

    // 0.17.6a: 临时对话按钮
    const ephemeralBtn = document.getElementById("chat-sidebar-ephemeral");
    if (ephemeralBtn) {
        ephemeralBtn.addEventListener("click", () => handleNewEphemeralConversation());
    }

    // 0.17.6a: "转为持久"按钮
    const promoteBtn = document.getElementById("chat-promote-ephemeral");
    if (promoteBtn) {
        promoteBtn.addEventListener("click", () => handlePromoteEphemeral());
    }

    // 注册事件监听
    await ipc.listenChatStream(handleStreamEvent);
    await ipc.listenChatConfirm(handleConfirmEvent);
    await ipc.listenChatTitleUpdated(handleTitleUpdated);
    await ipc.listenContextStatus(handleContextStatus);
    await ipc.listenChatCompactJob(handleCompactJob);
    await ipc.listenSkillActivated(handleSkillActivated);
    await ipc.listenVoicePartial(handleVoicePartial);
    await ipc.listenVoiceRecordingStart(handleVoiceRecordingStart);
    await ipc.listenVoiceRecordingEnd(handleVoiceRecordingEnd);
    await ipc.listenVoiceError(handleVoiceError);
    await ipc.listenVoiceLevel(handleVoiceLevel);
    await ipc.listenVoiceStatus(handleVoiceStatus);

    // 0.17.6a: promote 临时对话后，后端 emit CHAT_LOAD_CONVERSATION 通知切换
    listen(EVENTS.CHAT_LOAD_CONVERSATION, async (event) => {
        const convId = event?.payload;
        if (typeof convId === "string" && convId) {
            await handleSwitchConversation(convId);
        }
    });

    // 初始状态：检查 provider 配置 + 加载模型选择器
    try {
        const status = await ipc.getChatStatus();
        state.setProviderConfigured(status.provider_configured);

        // 0.13.6: 加载初始上下文窗口状态（即使为 null 也显示空环）
        try {
            const ctxStatus = await ipc.getContextWindowStatus(state.conversationId);
            components.updateContextIndicator(ctxStatus || null);
            if (ctxStatus) {
                components.renderContextWarning(ctxStatus.usage_percent, ctxStatus.summary_enabled, handleContextWarningAction);
            }
        } catch (e) {
            // 即使获取失败也显示空环
            components.updateContextIndicator(null);
            console.warn("[chat] 加载上下文窗口状态失败:", e);
        }
        updateProviderLabel(status);
        components.renderEmptyState(status.provider_configured, openSettings);
    } catch (e) {
        console.error("[chat] getChatStatus 失败:", e);
        components.renderEmptyState(false, openSettings);
    }
    refreshModelSelector();

    // 0.13.0：加载 tool 池规模 + MCP tool 名称（供工具卡片来源标记）
    // 注意：此处不触发 MCP 连接——预热阶段窗口不可见，无需拉起 MCP server 子进程。
    // MCP 后台预热在窗口首次获得焦点（真正显示给用户）时由 focus 事件触发。
    refreshToolPool(true);

    // Composer bar 悬浮预览 popup 初始化
    initComposerBarPopup();

    // 侧边栏 toggle
    const sidebarToggle = document.getElementById("chat-sidebar-toggle");
    if (sidebarToggle) {
        sidebarToggle.addEventListener("click", () => toggleSidebar());
    }

    // 模型选择器交互（0.21.23 下沉 model-selector.js，含供应商 flyout 定位）
    bindModelSelector();
    // 0.21.17：思考控件交互（支持等级 → 强度下拉，否则 → 简单开关）；
    // 0.27.7 起弹层互斥统一走 composer-popups 仲裁，不再注入 beforeOpen
    initThinkingSelector();

    // 0.27.7：斜杠命令面板（/new /model /tools /skill /mcp，phase §5.7/§8.3）
    // 0.27.8：/compact /prompt（phase §5.8）
    initCommandPalette({
        onCommand: handleSlashCommand,
        getInputRevision: composerInputRevision,
        insertTextAtCursor: insertAtCursor,
    });
    // 0.27.9 @ 附件面板（与命令面板同一互斥仲裁；§九）
    initAttachmentPalette({
        getConversationId: () => state.conversationId,
        getInputRevision: composerInputRevision,
        addAttachments,
        // @ 触发采纳成功后清除输入框触发文本（按钮打开路径不触发）。
        // 条件清除：异步签发期间用户可能已开始正常输入——只在输入仍是
        // @ 触发形态时清空，避免误清用户新输入
        clearTriggerText: () => {
            const input = document.getElementById("chat-input");
            if (input && splitAtQuery(input.value)) clearInput();
        },
    });

    // 0.27.8：提示词快捷条按钮（与 /prompt 同入口、同数据源）。
    // 打开后焦点回 composer——面板键盘路由依赖 textarea keydown（§九）。
    const promptBtn = document.getElementById("chat-prompt-btn");
    if (promptBtn) {
        promptBtn.title = t("chat.cmd.prompt.btn_title");
        promptBtn.addEventListener("click", (e) => {
            // 阻止冒泡到 document 的 click-outside 关闭逻辑（面板开即关）
            e.stopPropagation();
            focusInput();
            openPromptPalette();
        });
    }

    // 0.27.8：提示词库变更 → 面板缓存失效（打开中则原地刷新由下次打开自然发生）
    listen(EVENTS.PROMPT_LIBRARY_CHANGED, (e) => {
        if (e?.payload?.scope === "chat" || e?.payload?.scope === undefined) {
            invalidatePromptCache();
        }
    });

    // 0.12.7 §1：面包屑标题编辑（事件委托）
    bindBreadcrumbEdit();

    // 0.12.7 §6.5：系统提示词横幅交互
    bindPromptBanner();

    // Esc 键：生成中 → abort；空闲 → 隐藏窗口
    document.addEventListener("keydown", handleEsc);

    // 0.12.6：重试按钮（assistant 消息的 retry action）
    document.addEventListener("chat:retry", handleRetry);

    // 0.19.11: 只有首次真正可见的 focus 发起一次非阻塞后台预热；普通 focus
    // 仅刷新只读 tool snapshot，不再自动重试失败 server。
    window.addEventListener("focus", () => {
        focusInput();
        const firstVisibleActivation = !windowActivated;
        windowActivated = true;
        if (firstVisibleActivation) {
            ipc.ensureMcpConnected().catch(() => {
            });
            // 0.27.12：侧边栏 0.26.18 起默认展开，但列表加载只在发送/切换等
            // 事件路径触发——默认展开的初始渲染此前无人拉取（toggle 展开
            // 路径有）。首次真正可见时加载一次，preheat 的隐藏 init 不触发。
            refreshSidebar();
        }
        refreshToolPool(true);
        invalidateComposerBarCache();
        refreshPopupIfVisible();
    });

    // 初始聚焦
    focusInput();
}

// ── 发送 ────────────────────────────────────────

/**
 * 渲染一条用户消息的完整展示（0.23.12：附件独立成卡片气泡）。
 *
 * - 有附件：先渲染附件卡片（每个文件一张，右对齐静音卡片）；
 *   纯附件消息的持久化正文是"（音频附件：…）"人类可读摘要（FTS/导出/标题
 *   种子用），卡片已展示文件名，文本气泡不再重复渲染。
 * - 无附件：正文即文本气泡。
 *
 * @param {string} text 持久化正文
 * @param {Array<{kind?: string, name: string}>} [badges] 附件徽标（kind+name，无 ref）
 */
function renderUserEntry(text, badges = [], userMessageId = null) {
    if (badges.length > 0) {
        components.renderUserAttachmentCards(badges, userMessageId);
        if (text && text !== attachmentSummaryText(badges)) {
            components.renderUserMessage(text, userMessageId);
        }
        return;
    }
    if (text) {
        components.renderUserMessage(text, userMessageId);
    }
}

/**
 * 渲染持久化的失败轮（0.27.12）：错误说明行 + 「重试」按钮。
 * 重试预填草稿，不自动发送；历史附件无会话内快照时提示显式重附。
 * @param {string} errorText
 * @param {string} userText
 */
function renderFailedTurnError(errorText, userText, userMessageId) {
    const el = components.renderSignal(
        errorText ? `${t("chat.failed.prefix")}${errorText}` : t("chat.failed.prefix_no_reason"),
        "error",
    );
    if (!el) return;
    el.dataset.userMessageId = userMessageId;
    const retry = document.createElement("button");
    retry.type = "button";
    retry.className = "chat-context-warning-btn";
    retry.style.marginLeft = "8px";
    retry.style.flexShrink = "0";
    retry.textContent = t("chat.failed.retry");
    retry.addEventListener("click", () => {
        setInputValue(userText);
        const message = state.messages.find(msg => msg.id === userMessageId);
        const attachments = turnRetries.get(userMessageId, message?.attachments);
        if (attachments === null) components.renderErrorMessage(t("chat.attach.retry_expired"));
        else addAttachments(attachments);
        focusInput();
    });
    el.appendChild(retry);
}

/**
 * 发送一条用户消息。
 *
 * 0.23.12：composer 传结构化负载 `{text, attachments}`——text 是用户可见/
 * 持久化正文（气泡、state、标题），attachments 是本轮附件元数据（只注入
 * 本轮模型输入，不进入气泡/标题/历史）。编辑重发/重试同样传完整负载。
 *
 * 0.27.9：attachments 为 `{kind, ref, display}` 类型化元数据（§8.2）；
 * 徽标（kind+name）进气泡渲染与 state，ref 只随请求给后端注入本轮模型输入。
 *
 * @param {{text: string, attachments?: Array<{kind: string, ref: string, display: string}>}|string} input
 * @param {boolean} [isEdit] 编辑重发/重试（跳过截断标题逻辑）
 */
async function handleSend(input, isEdit = false) {
    const payload = typeof input === "string"
        ? {text: input, attachments: []}
        : {text: input?.text ?? "", attachments: input?.attachments ?? []};
    if (!payload.text) return;

    // 0.27.10 前端预检（§3.4 双校验）：图片附件 + 当前模型未启用视觉 →
    // 阻止发送并提示（后端发送校验为最终防线）；草稿与附件保留。
    const hasImage = payload.attachments.some((a) => a.kind === "image");
    if (hasImage && !currentModelSupportsVision()) {
        components.renderErrorMessage(t("chat.attach.vision_blocked"));
        return;
    }

    const conversationId = state.conversationId;
    // 移除空状态
    components.removeEmptyState();

    // 添加用户消息：附件卡片 + 正文气泡（徽标只含 kind+name，ref 不进 DOM/正文）
    const badges = payload.attachments.map((a) => ({kind: a.kind, name: a.display}));
    // 预填失败轮后确认发送：后端会复用同文尾部 user，前端也复用同一轮。
    const tail = state.messages.at(-1);
    const reuseFailed = tail?.role === "user" && tail.failed && tail.content === payload.text;
    const userMessage = reuseFailed ? tail : state.addMessage({role: "user", content: payload.text, attachments: badges});
    if (reuseFailed) {
        document.querySelectorAll(`[data-user-message-id="${userMessage.id}"]`).forEach(el => el.remove());
        userMessage.attachments = badges;
        userMessage.failed = false;
    }
    turnRetries.remember(userMessage.id, payload.attachments);
    renderUserEntry(payload.text, badges, userMessage.id);
    // 用户发消息 → 强制滚到底部（重置上滚标记）
    forceScrollToBottom();


    // 切换到流式模式
    setStreamingMode();
    state.setStreaming(true);
    state.resetStreamBuffer();

    // 创建 assistant 消息 DOM
    currentAssistantEl = components.createAssistantMessage(lastUserId());
    earlyStreamBuffer.begin(state.conversationId);

    try {
        // 0.17.6a: 临时对话模式传 ephemeral:true
        // 0.21.17: 支持等级的模型显式传 reasoning_effort；否则退回 thinkingEnabled
        const thinkingEnabled = effectiveThinkingEnabled();
        const reasoningEffort = state.supportsEffort ? state.thinkingEffort : null;
        const opts = state.ephemeralMode
            ? {ephemeral: true, targetWindow: "chat", thinkingEnabled, reasoningEffort}
            : {thinkingEnabled, reasoningEffort};
        // 0.23.12：附件元数据随本轮请求给后端（后端只注入本轮模型输入）
        if (payload.attachments.length > 0) {
            opts.attachments = payload.attachments;
        }
        const requestId = await ipc.chatPrompt(conversationId, payload.text, state.currentGroupId, opts);
        if (state.conversationId !== conversationId || !state.messages.includes(userMessage)) {
            ipc.chatAbort(requestId).catch(() => {});
            return;
        }
        state.setActiveRequestId(requestId);
        for (const payloadChunk of earlyStreamBuffer.resolve(requestId)) dispatchStreamPayload(payloadChunk);

        // 0.12.4 §6.7：新对话首条消息 → 截断生成标题（编辑重发不触发）
        // 0.21.16: 挪到 chatPrompt 成功之后——后端已预写对话记录（发出即保存），
        // rename 才能命中；此前在 prompt 前调用是 no-op。
        // 0.17.6a: 临时对话跳过标题生成（不写 SQLite，promote 后由主窗口负责）
        // 0.23.12：标题输入 = 用户可见正文，不出现 rref_/技术提示
        const isNewConversation = !reuseFailed && !isEdit && state.messages.length === 1;
        if (isNewConversation && !state.ephemeralMode) {
            const truncatedTitle = payload.text.slice(0, 20) + (payload.text.length > 20 ? "…" : "");
            try {
                await ipc.renameChatConversation(conversationId, truncatedTitle);
                if (state.conversationId !== conversationId) return;
                await updateBreadcrumb(truncatedTitle);
                // 0.12.5 §5.3：异步触发 LLM 命名（不等待，失败静默降级保持截断标题）
                ipc.generateConversationTitle(state.conversationId, payload.text).catch((e) => {
                    console.warn("[chat] LLM 标题生成失败:", e);
                });
            } catch (e) {
                console.warn("[chat] 截断标题设置失败:", e);
            }
        }
    } catch (e) {
        if (state.conversationId !== conversationId || !state.messages.includes(userMessage)) return;
        earlyStreamBuffer.clear();
        // 启动失败可能已预写失败轮，也可能尚未落库。只回滚未落库的乐观展示。
        if (!state.ephemeralMode) {
            try {
                const snapshots = await ipc.getChatMessages(conversationId);
                if (state.conversationId !== conversationId || !state.messages.includes(userMessage)) return;
                const persisted = resolvePersistedTurn(state.messages, userMessage.id, snapshots);
                userMessage.messageId = persisted;
                userMessage.failed = true;
            } catch {
                if (state.conversationId !== conversationId || !state.messages.includes(userMessage)) return;
                const index = state.messages.indexOf(userMessage);
                if (index >= 0) state.messages.splice(index, 1);
                turnRetries.delete(userMessage.id);
                document.querySelectorAll(`[data-user-message-id="${userMessage.id}"]`).forEach(el => el.remove());
            }
        } else {
            state.messages.splice(state.messages.indexOf(userMessage), 1);
            turnRetries.delete(userMessage.id);
            document.querySelectorAll(`[data-user-message-id="${userMessage.id}"]`).forEach(el => el.remove());
        }
        console.error("[chat] chatPrompt 失败:", e);
        // 移除空的 assistant 消息 DOM（createAssistantMessage 已创建但 chatPrompt 失败）
        if (currentAssistantEl) {
            currentAssistantEl.remove();
            currentAssistantEl = null;
        }
        // 友好化错误消息（0.12.5 §5.3）
        const errStr = String(e);
        let friendlyMsg;
        if (errStr.includes("AlreadyActive") || errStr.includes("已有对话请求")) {
            friendlyMsg = "已有对话正在生成，请等待完成或停止后再发送";
        } else if (errStr.includes("NotConfigured") || errStr.includes("未配置")) {
            friendlyMsg = "AI 服务未配置，请在设置中添加供应商和模型";
        } else if (errStr.includes("Timeout") || errStr.includes("超时")) {
            friendlyMsg = "AI 响应超时，请检查网络连接或模型状态";
        } else {
            friendlyMsg = `发送失败: ${errStr}`;
        }
        components.renderErrorMessage(friendlyMsg);
        finishStreaming();
        // 恢复输入框（不清空，让用户可以修改后重试）
        const input = document.getElementById("chat-input");
        if (input) input.disabled = false;
        refreshSidebar();
        return;
    }

    if (state.conversationId !== conversationId || !state.messages.includes(userMessage)) return;
    clearInput();
    // 0.23.12：附件随消息发出后离开输入框（与输入清空同一成功时机；
    // 只移除本次发送的 ref，发送间隙新附的文件不受影响）。后端 ref 不撤销——
    // 本轮 agent 仍要消费，生命周期由会话切换/新对话按 owner 统一回收。
    clearSentAttachments(payload.attachments);
    // 刷新侧边栏（更新 last_active_at + 标题）
    // 注意：对话持久化由 rig memory.append 异步完成，done chunk 到达后再刷新一次
    refreshSidebar();
    setActiveConversation(state.conversationId);
}

// ── 消息编辑重发（0.12.5 §5.5） ────────────────

/**
 * 消息编辑重发：截断后续消息（DB + state）→ 重新渲染 → 重新发送。
 * @param {string} userMessageId 被编辑轮的稳定用户消息标识
 * @param {string} newText 编辑后的文本
 */
async function handleEditMessage(userMessageId, newText) {
    if (editingTurn) return;
    const msgIndex = state.messages.findIndex(msg => msg.id === userMessageId && msg.role === "user");
    if (msgIndex < 0) return;
    const original = state.messages[msgIndex];
    const conversationId = state.conversationId;
    const attachments = turnRetries.get(userMessageId, original.attachments);
    if (attachments === null) {
        setInputValue(newText);
        focusInput();
        components.renderErrorMessage(t("chat.attach.retry_expired"));
        return;
    }
    if (attachments.some(a => a.kind === "image") && !currentModelSupportsVision()) {
        components.renderErrorMessage(t("chat.attach.vision_blocked"));
        return;
    }
    editingTurn = true;
    try {
        if (state.isStreaming) await handleStop();
        // 资源过期先阻止重发，不先破坏原历史。最终读取仍由后端再次校验。
        if (attachments.length) {
            const candidates = await ipc.attachmentListCandidates(conversationId);
            const valid = new Set((candidates.sessionRefs || []).map(a => a.ref));
            if (attachments.some(a => !valid.has(a.ref))) {
                if (state.conversationId === conversationId) {
                    setInputValue(newText);
                    focusInput();
                    components.renderErrorMessage(t("chat.attach.retry_expired"));
                }
                return;
            }
        }
        let messageId = null;
        const ephemeral = state.ephemeralMode;
        if (!ephemeral) {
            const snapshots = await ipc.getChatMessages(conversationId);
            messageId = resolvePersistedTurn(state.messages, userMessageId, snapshots);
        }
        if (state.conversationId !== conversationId || state.messages[msgIndex] !== original) return;
        const fromEnd = ephemeral ? state.messages.slice(msgIndex + 1).filter(msg => msg.role === "user").length : null;
        await ipc.truncateMessages(conversationId, 0, messageId, fromEnd);
        if (state.conversationId !== conversationId || state.messages[msgIndex] !== original) return;
        for (const msg of state.messages.slice(msgIndex)) turnRetries.delete(msg.id);
        state.messages.length = msgIndex;
        components.clearMessages();
        components.removeSummarySeparator();
        components.updateContextIndicator(null);
        invalidateComposerBarCache();
        let turnId = null;
        for (const msg of state.messages) {
            if (msg.role === "user") {
                turnId = msg.id;
                renderUserEntry(msg.content, msg.attachments ?? [], turnId);
            } else if (msg.role === "assistant") {
                const el = components.createAssistantMessage(turnId);
                if (el) components.finalizeAssistantMessage(el, msg.content, msg.thinking || "");
            }
        }
        components.renderSignal("消息已编辑", "info");
        // 发送失败时恢复这轮草稿，保留重新附加/修改的入口。
        setInputValue(newText);
        addAttachments(attachments);
        await handleSend({text: newText, attachments}, true);
    } catch (error) {
        console.error("[chat] 编辑重发失败:", error);
        if (state.conversationId === conversationId) components.renderErrorMessage(String(error));
    } finally {
        editingTurn = false;
    }
}

/** 气泡直接携带所属 user 的稳定标识，不依赖 DOM/存储行数。 */
async function handleRetry(e) {
    if (state.isStreaming) return;
    const anchor = e?.detail?.msgEl;
    const userMessageId = anchor?.dataset?.userMessageId || anchor?.closest("[data-user-message-id]")?.dataset.userMessageId;
    // 有锚点但找不到对应轮时必须停止，不能误重试最后一轮。
    const user = anchor
        ? state.messages.find(msg => msg.id === userMessageId && msg.role === "user")
        : state.messages.findLast(msg => msg.role === "user");
    if (user) await handleEditMessage(user.id, user.content);
}

// ── 停止 ────────────────────────────────────────

async function handleStop() {
    if (state.activeRequestId != null) {
        try {
            await ipc.chatAbort(state.activeRequestId);
        } catch (e) {
            console.warn("[chat] chatAbort 失败:", e);
        }
    }
    // 立即结束流式状态，不依赖后端兜底 Done chunk。
    // abort 后后端的兜底 Done 会被 request_id 校验过滤（activeRequestId 已清空）。
    // 0.18.0: trim 判断——纯空白 text/thinking 走 remove 分支
    if (currentAssistantEl && ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim())) {
        components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
        state.addMessage({role: "assistant", content: state.streamBuffer});
    } else if (currentAssistantEl) {
        // 无内容的空气泡直接移除
        currentAssistantEl.remove();
    }
    if (currentToolEl) {
        components.finalizeToolStatus(currentToolEl, true);
        currentToolEl = null;
    }
    // 0.12.7 §6.3：停止生成 Signal
    components.renderSignal("已停止生成", "info");
    finishStreaming();
}

// ── 流式事件处理 ────────────────────────────────

function handleStreamEvent(event) {
    const payload = event.payload;
    const {request_id, conversation_id} = payload;
    if (state.activeRequestId == null && earlyStreamBuffer.capture(payload)) return;

    // 忽略不属于当前请求的 chunk
    if (request_id !== state.activeRequestId) return;
    if (conversation_id !== state.conversationId) return;

    dispatchStreamPayload(payload);
}

function dispatchStreamPayload(payload) {
    const {chunk} = payload;

    switch (chunk.kind) {
        case "thinking":
            // 未启用深度思考时丢弃 thinking chunk（0.21.17：按有效思考状态判定）
            if (!effectiveThinkingEnabled()) break;
            state.appendThinkingBuffer(chunk.text);
            state.setThinking(true);
            // 首条 thinking 时创建 assistant DOM
            if (!currentAssistantEl) {
                currentAssistantEl = components.createAssistantMessage(lastUserId());
            }
            scheduleRender();
            break;

        case "text":
            // 首条 text 到达时标记 thinking 结束（折叠）
            if (state.isThinking) {
                state.setThinking(false);
            }
            // tool_call 后 currentAssistantEl 被置 null，新 text 到达时需创建新气泡
            if (!currentAssistantEl) {
                currentAssistantEl = components.createAssistantMessage(lastUserId());
            }
            state.appendStreamBuffer(chunk.text);
            scheduleRender();
            break;

        case "tool_call":
            // 结束当前 assistant 消息的流式状态（可能有 thinking 而无 text）
            // 0.18.0: trim 判断——纯空白 text/thinking 走 remove 分支
            if (currentAssistantEl && ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim())) {
                components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
                state.addMessage({role: "assistant", content: state.streamBuffer});
                state.resetStreamBuffer();
                state.resetThinkingBuffer();
                currentAssistantEl = null;
            } else if (currentAssistantEl) {
                // 模型直接 tool_call（无 text/thinking）：移除空的 typing 占位气泡
                currentAssistantEl.remove();
                currentAssistantEl = null;
            }
            // 显示 Tool 状态卡（0.12.2：按 call_id 跟踪，供 ToolResult 配对）
            currentToolEl = components.renderToolStatus(chunk.tool, chunk.arguments);
            if (chunk.call_id) {
                state.trackToolCall(chunk.call_id, {el: currentToolEl, tool: chunk.tool});
            }
            break;

        case "tool_result": {
            // 0.12.2 §4.7：tool 执行结果，按 call_id 配对到对应卡片
            const entry = chunk.call_id ? state.getToolCall(chunk.call_id) : null;
            const targetEl = entry?.el || currentToolEl;
            if (targetEl) {
                components.finalizeToolStatus(targetEl, chunk.success);
                components.appendToolResult(targetEl, chunk.summary, chunk.success);
            }
            break;
        }

        case "max_turns_reached":
            // 0.12.3 Phase C: tool loop 触顶提示
            if (currentAssistantEl) {
                components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
                state.addMessage({role: "assistant", content: state.streamBuffer});
            }
            components.renderSignal(`已达工具调用上限（${chunk.max_turns}轮），请缩减任务或开启新对话`, "warning");
            finishStreaming();
            break;

        case "done":
            finalizeDone(chunk);
            break;

        case "error": {
            const failedUser = state.messages.findLast(msg => msg.role === "user");
            if (failedUser) failedUser.failed = true;
            // 移除空的 streaming assistant DOM（有内容则先 finalize）
            if (currentAssistantEl) {
                // 0.18.0: trim 判断——纯空白走 remove 分支
                if ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim()) {
                    components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
                    state.addMessage({role: "assistant", content: state.streamBuffer, thinking: state.thinkingBuffer});
                } else {
                    currentAssistantEl.remove();
                }
            }
            components.renderErrorMessage(chunk.message);
            finishStreaming();
            break;
        }
    }
}

/**
 * requestAnimationFrame 节流渲染。
 */
function scheduleRender() {
    if (rafHandle) return;
    rafHandle = requestAnimationFrame(() => {
        rafHandle = 0;
        if (currentAssistantEl) {
            const thinkingDone = !state.isThinking && state.thinkingBuffer.length > 0;
            components.updateAssistantMessage(
                currentAssistantEl,
                state.streamBuffer,
                state.thinkingBuffer,
                thinkingDone
            );
        }
    });
}

/**
 * Done：最终渲染 + 清理（0.12.2：chunk 携带 token 用量）。
 * @param {{input_tokens: number, output_tokens: number}} chunk
 */
function finalizeDone(chunk) {
    // 0.18.0: 空内容（纯空白）时不 finalize，直接移除元素，不添加空消息到 state
    if (currentAssistantEl) {
        if ((state.streamBuffer || "").trim() || (state.thinkingBuffer || "").trim()) {
            components.finalizeAssistantMessage(currentAssistantEl, state.streamBuffer, state.thinkingBuffer);
            state.addMessage({role: "assistant", content: state.streamBuffer});
        } else {
            currentAssistantEl.remove();
        }
    }
    // 完成 Tool 状态卡（若有未配对到 result 的卡片，默认成功）
    if (currentToolEl) {
        components.finalizeToolStatus(currentToolEl, true);
        currentToolEl = null;
    }
    // 0.12.3：在最后一条 assistant 消息底部显示模型名
    if (chunk && chunk.model_name) {
        const targetEl = currentAssistantEl || components.getLastAssistantEl?.();
        if (targetEl) {
            components.renderModelLabel(targetEl, chunk.model_name);
        }
    }
    // 0.12.2 §4.8：在最后一条 assistant 消息底部显示 token 用量
    // 0.21.17: reported=false 表示供应商未报告 usage，不显示"0 token"误导用户
    if (chunk && chunk.reported && (chunk.input_tokens || chunk.output_tokens)) {
        // currentAssistantEl 已 null（如纯 tool 调用后 done）时，回查 DOM 最后一条 assistant
        const targetEl = currentAssistantEl || components.getLastAssistantEl?.();
        if (targetEl) {
            components.renderTokenUsage(targetEl, chunk.input_tokens, chunk.output_tokens);
        }
        state.setLastUsage({
            input_tokens: chunk.input_tokens,
            output_tokens: chunk.output_tokens,
            total_tokens: chunk.total_tokens || 0,
            reasoning_tokens: chunk.reasoning_tokens || 0,
            cached_input_tokens: chunk.cached_input_tokens || 0,
            reported: true,
        });
    }
    // 0.13.6: 记忆召回 badge
    if (state.lastRecallCount > 0) {
        const targetEl = currentAssistantEl || components.getLastAssistantEl?.();
        if (targetEl) {
            components.renderRecallBadge(targetEl, state.lastRecallCount);
        }
    }
    finishStreaming();
    // 持久化已完成（rig memory.append 在 done 前执行），刷新侧边栏更新 last_active_at
    refreshSidebar();
}

/**
 * 结束流式状态，恢复 composer。
 */
function finishStreaming() {
    state.setStreaming(false);
    state.setActiveRequestId(null);
    state.resetStreamBuffer();
    state.resetThinkingBuffer();
    state.clearToolCalls();
    currentAssistantEl = null;
    currentToolEl = null;
    setInputMode();
}

// ── 确认事件处理 ────────────────────────────────

function handleConfirmEvent(event) {
    const payload = event.payload;
    // 忽略不属于当前请求的确认
    // request_id=0 是 ChatService 未注册或无 active 请求时的降级值，仍处理以确保用户可操作
    if (payload.request_id !== 0 && payload.request_id !== state.activeRequestId) return;
    // 切换对话时 abort 与状态更新存在极短竞态窗口；同时校验 conversation_id，
    // 防止旧请求的确认卡片落入刚切换的新对话。
    if (payload.conversation_id && payload.conversation_id !== state.conversationId) return;

    components.renderConfirmCard(payload, async (confirmId, approved) => {
        try {
            await ipc.confirmChatAction(confirmId, approved);
        } catch (e) {
            console.error("[chat] confirmChatAction 失败:", e);
        }
    });
}

// ── 标题自动更新（0.12.5 §5.3） ────────────────

/**
 * 对话标题自动更新事件处理（0.12.5 §5.3）。
 * LLM 生成标题后后端 emit `chat-title-updated`，前端更新 header + 刷新侧边栏。
 */
async function handleTitleUpdated(event) {
    const {conversation_id, title} = event.payload;
    if (conversation_id === state.conversationId) {
        await updateBreadcrumb(title);
    }
    refreshSidebar();
}

// ── 上下文窗口状态（0.13.6） ───────────────────

/**
 * 上下文窗口状态更新事件处理（0.13.6）。
 * 后端在每次 prompt 前计算 token 估算 + 压缩/召回统计并推送。
 */
function handleContextStatus(event) {
    const status = event.payload;
    components.updateContextIndicator(status);
    components.renderContextWarning(status.usage_percent, status.summary_enabled, handleContextWarningAction);
    // 0.13.6: 保存 recall_count 供 finalizeDone 渲染
    state.setLastRecallCount(status.last_recall_count || 0);
    // 0.21.19: 有摘要覆盖消息时渲染折叠分隔线；回落 0 时移除旧分隔线防残留
    if (status.summarized_count > 0) {
        renderSummarySeparatorIfNeeded(status.summarized_count);
    } else {
        components.removeSummarySeparator();
    }
    // MCP 拓扑可以在后台预热或 prompt 准备时变化，
    // context-status 事件在 ensure_provider 之后推送，此时 tool 池可能已变，
    // 刷新 composer bar 文本 + 清除 popup 缓存。
    refreshToolPool(true);
    invalidateComposerBarCache();
    refreshPopupIfVisible();
}

/**
 * 0.21.19: 异步获取摘要内容并渲染折叠分隔线。
 * 竞态防护：响应回来时校验仍是当前对话。
 */
async function renderSummarySeparatorIfNeeded(summarizedCount) {
    const convId = state.conversationId;
    try {
        const summaryText = await ipc.getContextSummary(convId);
        if (state.conversationId !== convId || !summaryText) return;
        components.renderSummarySeparator(summaryText, summarizedCount);
    } catch (e) {
        // 摘要获取失败不阻塞对话
        console.warn("[chat] 获取摘要失败:", e);
    }
}

/**
 * 0.27.8: 手动压缩共用执行逻辑（/compact 与 80% 告警按钮，phase §5.8.4）。
 *
 * command 只做预检即返回；任务终态经 CHAT_COMPACT_JOB 事件按
 * job_id/conversation_id 推送（handleCompactJob 消费），不用超时猜成功。
 */
async function runManualCompact() {
    if (state.isStreaming) {
        components.renderSignal(t("chat.cmd.compact.busy"), "warning");
        return;
    }
    try {
        const result = await ipc.compactConversation(state.conversationId);
        switch (result?.status) {
            case "started":
                components.renderSignal(t("chat.cmd.compact.started"), "info");
                break;
            case "busy":
                components.renderSignal(t("chat.cmd.compact.busy"), "warning");
                break;
            case "in_flight":
                components.renderSignal(t("chat.cmd.compact.in_flight"), "info");
                break;
            case "no_content":
                components.renderSignal(t("chat.cmd.compact.no_content"), "info");
                break;
            case "noop":
                components.renderSignal(t("chat.cmd.compact.noop"), "info");
                break;
            default:
                break;
        }
    } catch (e) {
        console.error("[chat] 手动压缩失败:", e);
        components.renderSignal(t("chat.cmd.compact.failed", {msg: String(e)}), "warning");
    }
}

/**
 * 0.27.8: 手动压缩任务终态事件（按 conversation_id 过滤，只影响所属会话）。
 *
 * 成功后摘要分隔线/容量环由随后推送的 CHAT_CONTEXT_STATUS 更新
 * （任务收尾 emit_context_status_updated）。
 */
function handleCompactJob(event) {
    const payload = event.payload;
    if (!payload || payload.conversation_id !== state.conversationId) return;
    switch (payload.status) {
        case "succeeded":
            components.renderSignal(
                t("chat.cmd.compact.done", {
                    n: payload.covered_count ?? 0,
                    turns: payload.kept_turns ?? 2,
                }),
                "info"
            );
            break;
        case "no_op":
            components.renderSignal(t("chat.cmd.compact.noop"), "info");
            break;
        case "cancelled":
            components.renderSignal(t("chat.cmd.compact.cancelled"), "warning");
            break;
        case "failed":
            components.renderSignal(
                t("chat.cmd.compact.failed", {msg: payload.error || "unknown"}),
                "warning"
            );
            break;
        default:
            break; // running 不打扰
    }
}

/**
 * 压缩提示条操作回调（0.13.6 / 0.27.8）。
 * - compress: 调 runManualCompact——与 /compact 共用手动摘要语义
 *   （旧 compress_context_now「立即裁剪」链路已由 0.27.8 取代）
 */
async function handleContextWarningAction(action) {
    if (action === "compress") {
        await runManualCompact();
    }
}

// ── Skill 激活 Signal（0.13.6） ───────────────────

/**
 * Skill 激活事件处理（0.13.6）。
 * 后端在 resolve_skill_triggers 完成后推送，前端渲染 Signal 消息。
 */
function handleSkillActivated(event) {
    const {request_id, skills} = event.payload;
    // 忽略不属于当前请求的事件
    if (request_id !== state.activeRequestId) return;
    for (const skill of skills) {
        components.renderSignal(
            `Skill 已激活: ${skill.name} (${skill.source})`,
            "info"
        );
    }
}

// ── Alt 系统菜单抑制 ──────────────────────────────
// 无边框窗口按 Alt 键会触发 Win32 默认菜单（左上角弹出）。
// 拦截 Alt keydown 的默认行为，防止系统菜单弹出。
// 不影响 Alt 作为修饰键的功能（如 Alt+Space 语音输入由 hook 处理）。

document.addEventListener("keydown", (e) => {
    if (e.key === "Alt") {
        e.preventDefault();
    }
});

// ── Esc 键 ──────────────────────────────────────

function handleEsc(e) {
    if (e.key !== "Escape") return;
    // 0.27.7：命令面板打开时 Esc 只关面板，不隐藏窗口（§九：Esc 只关弹层）。
    // textarea 内的 Esc 已由面板 keydown 拦截；这里兜底焦点不在 composer 的场景。
    if (isPaletteOpen()) {
        closePalette();
        focusInput();
        return;
    }
    // 0.27.15 review：模型/思考下拉打开时 Esc 已被 flyout 键盘导航消费（仅关
    // 面板）——不得再隐藏窗口/中止生成（stopPropagation 不拦同节点监听）。
    if (wasConsumedByKeynav(e)) return;
    e.preventDefault();
    if (state.isStreaming) {
        handleStop();
    } else {
        ipc.hideChatWindow();
    }
}

// ── 语音输入（0.12.3 对齐主窗口：热键驱动，非 IPC 按钮）─────────────

/** voice-recording-start: 显示语音指示器（仅 target="chat"） */
function handleVoiceRecordingStart(event) {
    const payload = event.payload;
    if (payload?.target !== "chat") return;
    showVoiceIndicator();
}

/** voice-level: 波形动画（仅 target="chat"） */
function handleVoiceLevel(event) {
    const payload = event.payload;
    if (payload?.target !== "chat") return;
    updateVoiceLevel(payload?.level ?? 0);
}

/** voice-status: 模型加载中等状态提示（仅 target="chat"） */
function handleVoiceStatus(event) {
    const payload = event.payload;
    if (payload?.target !== "chat") return;
    if (payload?.message) {
        showVoiceStatus(payload.message);
    }
}

/** voice-partial: 实时更新 textarea（仅 target="chat"） */
function handleVoicePartial(event) {
    const payload = event.payload;
    if (payload?.target !== "chat") return;
    updateVoicePartial(payload);
}

/** voice-recording-end: 隐藏语音指示器 */
function handleVoiceRecordingEnd() {
    if (isVoiceRecording()) {
        hideVoiceIndicator();
    }
}

/** voice-error: 在语音指示器上显示错误（红色波形 + 错误文案） */
function handleVoiceError(event) {
    const payload = event.payload;
    if (payload?.target !== "chat") return;
    if (isVoiceRecording()) {
        hideVoiceIndicator();
    }
    if (payload?.message) {
        showVoiceError(payload.message);
    }
}

// ── 新对话 ──────────────────────────────────────

/**
 * 0.27.7/0.27.8 斜杠命令执行器（command-palette 采纳后分发，phase §8.3）。
 *
 * - /new：复用新对话链路；生成中禁用并提示先停止（不清输入外的任何状态）
 * - /model：打开与左下角同款模型 flyout（选择即切换对话窗口默认模型，
 *   与手动切换完全同效；首版不支持命令直接带参——2026-10-23 定案）
 * - /compact：手动压缩（与 80% 告警按钮共用 runManualCompact）
 * - /prompt：打开提示词选择（scope=chat；采纳由面板光标插入，不经此分发）
 * - /tools /mcp /skill 的面板内展示由 command-palette 自理，不经此分发
 * @param {string} name 命令名（不含 /）
 */
function handleSlashCommand(name) {
    switch (name) {
        case "new":
            if (state.isStreaming) {
                components.renderSignal(t("chat.cmd.new.streaming"), "warning");
                focusInput();
                return;
            }
            handleNewConversation();
            break;
        case "model":
            focusInput();
            openModelDropdown();
            break;
        case "compact":
            runManualCompact();
            break;
        case "prompt":
            focusInput();
            openPromptPalette();
            break;
        default:
            break;
    }
}

async function handleNewConversation(groupId = null) {
    if (state.isStreaming) {
        handleStop();
    }
    // 0.23.12：新对话 = 当前会话结束 → 撤销其未消费附件 ref
    ipc.revokeChatAudioAttachments(state.conversationId).catch((error) => {
        console.warn("revoke chat audio attachments failed:", error);
    });
    clearAttachments();
    turnRetries.clear();
    state.resetConversation();
    state.setCurrentGroupId(groupId);
    components.clearMessages();
    components.renderEmptyState(state.providerConfigured, openSettings);
    setActiveConversation(state.conversationId);
    await updateBreadcrumb("新对话");
    // 0.12.7 §6.5：新对话无提示词，隐藏横幅
    await updatePromptBanner(state.conversationId);
    updateEphemeralBadge();
    refreshSidebar();
    focusInput();
}

// ── 临时对话（0.17.6a）──────────────────────────

/**
 * 新建临时对话。
 * resetConversation + 标记 ephemeralMode=true + 显示"临时"badge。
 * 不刷新侧边栏（临时对话不出现在持久化列表中）。
 */
async function handleNewEphemeralConversation() {
    if (state.isStreaming) {
        handleStop();
    }
    // 0.23.12：同 handleNewConversation——撤销当前会话附件
    ipc.revokeChatAudioAttachments(state.conversationId).catch((error) => {
        console.warn("revoke chat audio attachments failed:", error);
    });
    clearAttachments();
    turnRetries.clear();
    state.resetConversation();
    state.setEphemeralMode(true);
    components.clearMessages();
    components.renderEmptyState(state.providerConfigured, openSettings);
    await updateBreadcrumb(t("ai.ephemeral_title"));
    await updatePromptBanner(state.conversationId);
    updateEphemeralBadge();
    // 不刷新侧边栏——临时对话不在列表中
    focusInput();
}

/**
 * 将当前临时对话提升为持久对话。
 * 调用后端 promote_ephemeral_conversation 后退出临时模式。
 */
async function handlePromoteEphemeral() {
    if (!state.ephemeralMode) return;
    try {
        await promoteEphemeralConversation(state.conversationId);
        // promote 成功：后端已写入 SQLite + 打开对话窗口（已是当前窗口）
        // 切换到持久模式（handleSwitchConversation 会被 CHAT_LOAD_CONVERSATION 事件触发）
        state.setEphemeralMode(false);
        updateEphemeralBadge();
        refreshSidebar();
    } catch (e) {
        console.error("[chat] promote 临时对话失败:", e);
        components.renderErrorMessage(String(e));
    }
}

/**
 * 更新标题栏"临时"badge + "转为持久"按钮显隐。
 */
function updateEphemeralBadge() {
    const badge = document.getElementById("chat-ephemeral-badge");
    const promoteBtn = document.getElementById("chat-promote-ephemeral");
    if (badge) badge.hidden = !state.ephemeralMode;
    if (promoteBtn) promoteBtn.hidden = !state.ephemeralMode;
}

/**
 * 切换到指定对话（0.12.3 Phase B）。
 * 停止当前生成 → 设置 conversation_id → 从后端加载历史消息 → 渲染。
 * @param {string} conversationId
 */
async function handleSwitchConversation(conversationId, groupId = null) {
    if (state.isStreaming) {
        handleStop();
    }

    // 0.23.12：离开旧会话 → 按 owner 批量撤销其未消费附件 ref（会话结束语义）
    if (conversationId !== state.conversationId) {
        const previousId = state.conversationId;
        ipc.revokeChatAudioAttachments(previousId).catch((error) => {
            console.warn("revoke chat audio attachments failed:", error);
        });
        clearAttachments();
    }

    // 更新 state（0.12.4 §6.1：用 setter 替代直接赋值，避免 ES module 只读绑定 TypeError）
    state.setConversationId(conversationId);
    state.setCurrentGroupId(groupId);
    state.setEphemeralMode(false); // 0.17.6a: 切换到持久对话，退出临时模式
    state.messages.length = 0;
    turnRetries.clear();
    state.setStreaming(false);
    state.setActiveRequestId(null);
    state.resetStreamBuffer();
    state.resetThinkingBuffer();
    state.clearToolCalls();

    components.clearMessages();
    setActiveConversation(conversationId);

    try {
        const messages = await ipc.getChatMessages(conversationId);
        if (state.conversationId !== conversationId) return;
        if (messages.length === 0) {
            components.renderEmptyState(state.providerConfigured, openSettings);
        } else {
            // 0.12.7 §6.4：时间分隔符——首条消息 + 间隔 >5 分钟时插入
            let lastTs = null;
            for (const msg of messages) {
                // 插入时间分隔符
                if (msg.created_at) {
                    if (lastTs === null || Math.abs(msg.created_at - lastTs) > 300) {
                        components.renderTimeSeparator(msg.created_at);
                    }
                    lastTs = msg.created_at;
                }

                if (msg.role === "user") {
                    // 0.27.9：历史加载渲染独立附件卡（DB 徽标 kind+name；兼容旧纯文件名=音频）
                    const user = state.addMessage({role: "user", content: msg.text, attachments: msg.attachments ?? [], messageId: msg.message_id, failed: !!msg.failed});
                    renderUserEntry(msg.text, msg.attachments ?? [], user.id);
                    // 0.27.12：失败轮持久化呈现——错误行 + 重试（预填原文不自动发送）
                    if (msg.failed) renderFailedTurnError(msg.error_text || "", msg.text, user.id);

                } else if (msg.role === "assistant") {
                    // 包含 tool_name 的 assistant 消息渲染为工具调用卡片
                    if (msg.tool_name && !msg.text) {
                        const toolEl = components.renderToolStatus(msg.tool_name, msg.tool_arguments, {skipTiming: true});
                        if (toolEl) {
                            components.finalizeToolStatus(toolEl, true);
                            // 有结果摘要时追加可折叠详情
                            if (msg.tool_result) {
                                components.appendToolResult(toolEl, msg.tool_result, true);
                            }
                        }
                        continue;
                    }
                    const el = components.createAssistantMessage(lastUserId());
                    if (el) {
                        components.finalizeAssistantMessage(el, msg.text, msg.thinking || "");
                        state.addMessage({role: "assistant", content: msg.text});
                    }
                }
            }
        }
    } catch (e) {
        if (state.conversationId !== conversationId) return;
        console.error("[chat] 加载对话历史失败:", e);
        components.renderEmptyState(state.providerConfigured, openSettings);
    }

    // 0.12.7 §1：切换对话后更新面包屑（使用 DB 中的 group_id 更可靠）
    const convs = await ipc.listChatConversations();
    if (state.conversationId !== conversationId) return;
    const conv = convs.find((c) => c.id === conversationId);
    state.setCurrentGroupId(conv?.group_id || groupId);
    await updateBreadcrumb(conv?.title || "新对话");
    // 0.12.7 §6.5：查询并显示分组系统提示词
    await updatePromptBanner(conversationId);
    updateEphemeralBadge(); // 0.17.6a: 确保 badge 状态正确

    // 0.21.18: 切换对话后刷新上下文容量指示器——之前不刷新，旧对话的百分比
    // 会残留到下一次 prompt；后端缓存 miss 时按需重算，历史对话也能显示。
    // 竞态防护：响应回来时校验仍是当前对话，快速连点切换不串台（§5.1）。
    try {
        const ctxStatus = await ipc.getContextWindowStatus(conversationId);
        if (state.conversationId === conversationId) {
            components.updateContextIndicator(ctxStatus || null);
            if (ctxStatus) {
                components.renderContextWarning(ctxStatus.usage_percent, ctxStatus.summary_enabled, handleContextWarningAction);
                // 0.21.19: 切换对话时也渲染摘要分隔线；无摘要则清掉旧分隔线
                if (ctxStatus.summarized_count > 0) {
                    renderSummarySeparatorIfNeeded(ctxStatus.summarized_count);
                } else {
                    components.removeSummarySeparator();
                }
            } else {
                components.removeSummarySeparator();
            }
        }
    } catch (e) {
        if (state.conversationId === conversationId) {
            components.updateContextIndicator(null);
        }
    }

    focusInput();
}

// ── 侧边栏重命名同步 ──────────────────────────

/**
 * 侧边栏重命名后同步更新 header 标题（仅当前活跃对话）。
 * @param {string} conversationId
 * @param {string} newTitle
 */
async function handleSidebarRenamed(conversationId, newTitle) {
    if (conversationId === state.conversationId) {
        await updateBreadcrumb(newTitle);
    }
}

// ── 导出对话（0.12.5 §5.6） ────────────────────

/**
 * 导出对话为 Markdown 文件。
 * 委托 ipc.exportConversation：加载消息 → 格式化 Markdown → Tauri save 对话框 → 写文件。
 * @param {string} conversationId
 * @param {string} title 对话标题（用于文件名和 Markdown 标题）
 */
async function handleExportConversation(conversationId, title) {
    try {
        await ipc.exportConversation(conversationId, title);
    } catch (e) {
        console.error("[chat] 导出对话失败:", e);
    }
}

// ── 面包屑标题（0.12.7 §1） ────────────────────────

/**
 * 构建分组路径（从根到当前分组的链路）。
 * @param {Array} groups 平铺分组列表
 * @param {string|null} groupId 当前分组 ID
 * @returns {Array<{id: string, name: string, systemPrompt: string|null}>} 路径段数组（从根到当前）
 */
function buildGroupPath(groups, groupId) {
    if (!groupId) return [];
    const map = new Map(groups.map(g => [g.id, g]));
    const path = [];
    let cur = groupId;
    while (cur) {
        const g = map.get(cur);
        if (!g) break;
        path.unshift({
            id: g.id,
            name: g.name,
            systemPrompt: g.system_prompt || null,
        });
        cur = g.parent_id || null;
    }
    return path;
}

/**
 * 更新面包屑标题：文件夹路径 + 对话标题。
 * 异步获取分组列表，构建从根到当前分组的路径，渲染为面包屑。
 * 仅对话标题（最后一段）可编辑，文件夹段为静态展示。
 * 直属分组有系统提示词时，在该段显示 accent 圆点指示器。
 * @param {string} title 对话标题
 */
async function updateBreadcrumb(title) {
    const breadcrumbEl = document.getElementById("chat-breadcrumb");
    if (!breadcrumbEl) return;

    // 编辑中不更新（避免破坏 input）
    if (breadcrumbEl.querySelector(".chat-title-edit-input")) return;

    // 获取分组列表，构建路径
    let path = [];
    if (state.currentGroupId) {
        try {
            const groups = await ipc.listConversationGroups();
            path = buildGroupPath(groups, state.currentGroupId);
        } catch (e) {
            console.warn("[chat] 获取分组列表失败:", e);
        }
    }

    // 渲染面包屑：文件夹段 + 分隔符 + 对话标题
    let html = "";
    for (let i = 0; i < path.length; i++) {
        const seg = path[i];
        const isDirectGroup = i === path.length - 1;
        // 仅直属分组显示系统提示词指示器（ancestor 的提示词不继承）
        const hasPromptCls = isDirectGroup && seg.systemPrompt ? " has-prompt" : "";
        const segTitle = seg.systemPrompt ? `${seg.name}（含系统提示词）` : seg.name;
        html += `<span class="chat-breadcrumb-segment${hasPromptCls}" title="${escapeAttr(segTitle)}">${escapeText(seg.name)}</span>`;
        html += `<span class="chat-breadcrumb-sep">/</span>`;
    }
    html += `<span class="chat-breadcrumb-title" id="chat-conversation-title" title="点击重命名">${escapeText(title || "新对话")}</span>`;
    breadcrumbEl.innerHTML = html;
}

/**
 * 绑定面包屑标题编辑（事件委托，支持 innerHTML 重渲染后仍生效）。
 * 点击对话标题段 → 内联编辑 → Enter 确认（重命名）/ Esc 取消。
 */
function bindBreadcrumbEdit() {
    const breadcrumbEl = document.getElementById("chat-breadcrumb");
    if (!breadcrumbEl) return;

    breadcrumbEl.addEventListener("click", (e) => {
        const titleEl = e.target.closest(".chat-breadcrumb-title");
        if (!titleEl) return;

        const oldTitle = titleEl.textContent;
        const input = document.createElement("input");
        input.type = "text";
        input.value = oldTitle;
        input.className = "chat-title-edit-input";
        titleEl.replaceWith(input);
        input.focus();
        input.select();

        let confirmed = false;

        /** 创建新的标题 span 替换 input */
        const restoreTitle = (text) => {
            const span = document.createElement("span");
            span.className = "chat-breadcrumb-title";
            span.id = "chat-conversation-title";
            span.textContent = text;
            span.title = "点击重命名";
            input.replaceWith(span);
        };

        const finishEdit = async () => {
            if (confirmed) return;
            confirmed = true;
            const newTitle = input.value.trim();
            if (newTitle && newTitle !== oldTitle) {
                try {
                    await ipc.renameChatConversation(state.conversationId, newTitle);
                    restoreTitle(newTitle);
                    refreshSidebar();
                } catch (err) {
                    console.error("[chat] 重命名失败:", err);
                    restoreTitle(oldTitle);
                }
            } else {
                restoreTitle(oldTitle);
            }
        };

        input.addEventListener("keydown", (ev) => {
            if (ev.key === "Enter") {
                ev.preventDefault();
                finishEdit();
            } else if (ev.key === "Escape") {
                confirmed = true;
                restoreTitle(oldTitle);
            }
        });
        input.addEventListener("blur", finishEdit);
    });
}

// ── 系统提示词横幅（0.12.7 §6.5） ──────────────

/**
 * 绑定提示词横幅交互（折叠/展开 + 关闭）。
 *
 * 横幅位置在消息区上方，可随消息区滚动（非 sticky）——用户看一眼即可滚走。
 * - toggle 按钮：展开/折叠 body
 * - close 按钮：本会话隐藏（state.dismissedPromptConvs 记录）
 */
function bindPromptBanner() {
    const banner = document.getElementById("chat-prompt-banner");
    if (!banner) return;

    const toggle = document.getElementById("chat-prompt-banner-toggle");
    toggle?.addEventListener("click", () => {
        if (banner.hasAttribute("data-collapsed")) {
            banner.removeAttribute("data-collapsed");
        } else {
            banner.setAttribute("data-collapsed", "");
        }
    });

    const close = document.getElementById("chat-prompt-banner-close");
    close?.addEventListener("click", () => {
        banner.hidden = true;
        if (state.conversationId) {
            state.dismissedPromptConvs.add(state.conversationId);
        }
    });
}

/**
 * 查询并更新提示词横幅。
 *
 * 切换对话时调用——查询 `get_conversation_system_prompt`，
 * 有提示词且未被本会话关闭则显示，否则隐藏。
 * @param {string} conversationId
 */
async function updatePromptBanner(conversationId) {
    const banner = document.getElementById("chat-prompt-banner");
    const body = document.getElementById("chat-prompt-banner-body");
    if (!banner || !body) return;

    if (!conversationId) {
        banner.hidden = true;
        return;
    }

    // 本会话已关闭
    if (state.dismissedPromptConvs.has(conversationId)) {
        banner.hidden = true;
        return;
    }

    try {
        const prompt = await ipc.getConversationSystemPrompt(conversationId);
        if (prompt && prompt.trim()) {
            body.textContent = prompt;
            banner.hidden = false;
            // 默认展开
            banner.removeAttribute("data-collapsed");
        } else {
            banner.hidden = true;
        }
    } catch (e) {
        banner.hidden = true;
    }
}

// ── 设置 ────────────────────────────────────────

function openSettings() {
    // 跳转到设置页「AI 供应商」tab（0.26.18 档位卡移入后，未配置供应商的
    // 空状态引导在此 tab 可完成 供应商→档位→启用 的完整链路）
    invoke("open_settings_tab", {tab: "ai-providers"});
}

function openProviderSettings() {
    invoke("open_settings_tab", {tab: "ai-providers"});
}

/**
 * 刷新 tool 池规模指示 + MCP tool 名称集合（0.13.0）。
 *
 * 加载内置 + MCP tool 数量，在 composer bar 显示「内置 N + MCP M = K tools」。
 * 同时加载 MCP tool 名称集合，供工具卡片渲染时标记来源。
 *
 * 0.13.8: 加 5 秒节流——窗口 focus 时无脑刷新会导致 3 个 IPC 频繁调用
 * （其中 get_mcp_tool_pool_size 内部还遍历所有 server），节流后只在
 * 首次 focus 或距上次刷新 >5s 时才发 IPC。
 */
let _refreshToolPoolLastTs = 0;
const _REFRESH_TOOL_POOL_THROTTLE_MS = 5000;

async function refreshToolPool(force = false) {
    // 节流：5 秒内不重复刷新（force=true 时跳过节流，供事件驱动的关键刷新用）
    const now = Date.now();
    if (!force && now - _refreshToolPoolLastTs < _REFRESH_TOOL_POOL_THROTTLE_MS) return;
    _refreshToolPoolLastTs = now;

    const el = document.getElementById("chat-tool-pool");
    try {
        const [size, names, sources] = await Promise.all([
            ipc.getMcpToolPoolSize(),
            ipc.getMcpToolNames(),
            ipc.getMcpToolSources(),
        ]);
        state.setMcpToolNames(names);
        state.setMcpToolSources(sources);

        // 隐藏 tool pool 文本——composer bar 只显示圆圈进度条
        // tool 数量信息已聚合到 hover popup 中展示
        if (el) el.classList.add('hidden');
    } catch (e) {
        console.error("[chat] refreshToolPool 失败:", e);
        if (el) el.classList.add('hidden');
    }
}

// ── 绑定 header 按钮 ───────────────────────────

function bindHeaderButtons() {
    const newBtn = document.getElementById("chat-new-btn");
    const providerSettingsBtn = document.getElementById("chat-provider-settings-btn");
    const settingsBtn = document.getElementById("chat-settings-btn");

    if (newBtn) newBtn.addEventListener("click", handleNewConversation);
    if (providerSettingsBtn) providerSettingsBtn.addEventListener("click", openProviderSettings);
    if (settingsBtn) settingsBtn.addEventListener("click", openSettings);

    // Titlebar 按钮
    const minimizeBtn = document.getElementById("titlebar-minimize");
    const maximizeBtn = document.getElementById("titlebar-maximize");
    const closeBtn = document.getElementById("titlebar-close");

    if (minimizeBtn) minimizeBtn.addEventListener("click", () => {
        getCurrentWindow().minimize();
    });
    if (maximizeBtn) maximizeBtn.addEventListener("click", async () => {
        const win = getCurrentWindow();
        const isMax = await win.isMaximized();
        isMax ? win.unmaximize() : win.maximize();
    });
    if (closeBtn) closeBtn.addEventListener("click", () => {
        ipc.hideChatWindow();
    });
}

// ── 启动 ────────────────────────────────────────

document.addEventListener("DOMContentLoaded", () => {
    bindHeaderButtons();
    init();
});
