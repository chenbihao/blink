//! AI 模式状态机 + 流式渲染 + 确认卡（0.17.6 B2.2 + B2.3）。
//!
//! 主窗口 AI 从 SearchService::trigger_ai 切换到 ChatService::prompt，
//! 与对话窗口统一调度模型。主窗口使用临时对话（EphemeralConversationMemory）。
//!
//! 状态机：
//!   SearchMode --Tab on AI Ghost--> AiMode --ESC--> SearchMode
//!   AiMode --失焦--> 窗口保持不隐藏（watchdog MAIN_WINDOW_AI_ACTIVE 跳过）
//!   AiMode --Alt+Space--> set_focus + #ai-query.focus()（不 toggle hide）
//!
//! 事件监听：
//!   CHAT_STREAM（按 request_id 过滤）
//!   CHAT_CONFIRM_ACTION（按 conversation_id 过滤）

import {listen} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {
    chatAbort,
    chatPrompt,
    clearMainAiActive,
    confirmChatAction,
    copyToClipboard,
    getEphemeralModels,
    promoteEphemeralConversation
} from "../shared/api.js";
import {initMarkdown, renderMarkdown, renderMarkdownStream} from "../shared/markdown.js";
import {
    createThrottledRenderer,
    renderConfirmCard,
    renderToolLine,
    renderToolResultLine,
    renderTypingIndicator,
} from "../shared/message-render.js";
import {iconHTML} from "../shared/icon.js";
import {
    aiContentEl,
    aiDisplayEl,
    aiModeEl,
    aiModelLabelEl,
    aiQueryEl,
    aiRoundsEl,
    aiToolLineEl,
    queryEl,
    searchModeEl,
} from "./dom.js";
import {resetMaxHeight, resyncWindowSize, syncWindowSize} from "./window-size.js";
import {t} from "../i18n/index.js";
import {createEarlyStreamBuffer} from "../shared/early-stream-buffer.js";
import * as ghost from "./ghost.js";
import * as suggestionBar from "./suggestion-bar.js";
import * as search from "./search.js";
import * as results from "./results.js";
import * as modeHeader from "./mode-header.js";
import * as dictation from "./dictation.js";

// ── 状态 ──────────────────────────────────────────────────────────────────────

let active = false;
let conversationId = null;
let requestId = null;
const earlyStreamBuffer = createEarlyStreamBuffer();
let streamBuffer = "";
let streamRenderer = null;
let throttle = null;
let awaitingConfirm = false;
let unlistenStream = null;
let unlistenConfirm = null;
let currentQuestion = ""; // 当前轮用户问题文本（折叠摘要用）
// 0.28.28：当前轮引用材料（划词组合提问）。问题与材料分开保存：
// 气泡分体渲染、发送时在模型输入边界组合（材料随消息进会话历史，
// 后续追问通过历史理解材料，不重复附加）。
let currentMaterial = null;
// 0.28.28：会话/发送代次——enter/exit 递增；异步返回（chatPrompt/定时器）
// 校验代次，退出、取消、提升到 Chat 后旧结果不得复活或污染新会话。
let sessionGen = 0;
// 0.28.28：启动中标志——request id 尚未返回的窗口期也能阻止重入；
// 迟到受理的请求在代次过期时被追认取消。
let sendInFlight = false;
// 0.28.28：待发消息（至多一条，可编辑/取消）。生成中 Enter 暂存，
// 本轮成功完成后（Ready 槽位信号）自动发送一次。
let pendingFollowup = null;
// 0.28.28：本轮终态类型（'done' | 'error' | 'max_turns'）。仅成功完成的
// Ready 才自动发送待发项；失败/取消/等待确认保留待发等待用户处理。
let lastTerminal = null;
// 0.28.28：后端请求槽已释放（Ready 到达）。终态事件与槽位释放之间存在时序差，
// requestId 保留到最后完成请求上以便 Ready 过滤；可续发判定以本标志为准。
let slotReady = false;

/** 用户是否手动上滚（不在底部）——为 true 时暂停自动滚到底部 */
let userScrolledUp = false;

/** @type {HTMLElement|null} "回到底部" 浮动按钮 */
let scrollBottomBtn = null;

// ── 初始化 ────────────────────────────────────────────────────────────────────

export function init() {
    initMarkdown();
    // 注册 CHAT_STREAM / CHAT_CONFIRM_ACTION 监听
    listen(EVENTS.CHAT_STREAM, handleStreamEvent);
    listen(EVENTS.CHAT_CONFIRM_ACTION, handleConfirmEvent);
    // 0.25.13: 注册 AI 追问框听写 sink——语音事件接线在 dictation.js
    dictation.registerSink("ai", aiDictationSink);
    // 监听 #ai-display 手动滚动（用户上滚时暂停自动跟随）
    bindScrollListener();
}

// ── AI 听写 sink（0.25.13）────────────────────────────────────────────────
// 追问框的语音文本落位。只落已定稿文本（confirmed）——追问框没有 ghost
// 预览层，未定稿 preview 不进真输入框；终稿只填入不自动发送，与手动输入
// 一致，由用户确认后回车提交（防误识别直接发出）。
const aiDictationSink = {
    currentText: () => aiQueryEl.value,

    indicatorEl: () => document.querySelector("#ai-input-bar .voice-indicator"),

    begin() {
        aiQueryEl.focus();
    },

    partial(view) {
        const text = view.text ?? view.confirmed ?? "";
        aiQueryEl.value = text;
        aiQueryEl.setSelectionRange(text.length, text.length);
    },

    final(text) {
        aiQueryEl.value = text;
        aiQueryEl.setSelectionRange(text.length, text.length);
        aiQueryEl.focus();
    },

    restore(baseText) {
        aiQueryEl.value = baseText;
        aiQueryEl.setSelectionRange(baseText.length, baseText.length);
    },

    settle(text) {
        if (aiQueryEl.value === text) return;
        aiQueryEl.value = text;
        aiQueryEl.setSelectionRange(text.length, text.length);
    },

    end() {},
};

/** 当前是否处于 AI 模式。 */
export function isActive() {
    return active;
}

/** 当前是否在等待确认（键盘 Enter 确认用）。 */
export function isAwaitingConfirm() {
    return awaitingConfirm;
}

/** 确认当前确认卡片（键盘 Enter 触发）。 */
export function confirmCurrentAction() {
    if (!awaitingConfirm) return;
    const approveBtn = aiContentEl.querySelector('[data-action="approve"]');
    if (approveBtn) {
        approveBtn.click();
    }
}

/**
 * 0.28.28：Enter 是否落在确认卡片上（明确聚焦该卡片时才允许键盘确认）。
 * 输入框的 Enter 遵循输入/待发语义，不能把补充提问误当工具批准。
 */
export function isConfirmCardEvent(e) {
    const card = aiContentEl.querySelector(".ai-confirm-card");
    if (!card) return false;
    return card.contains(e?.target) || card.contains(document.activeElement);
}

// ── 模式切换 ──────────────────────────────────────────────────────────────────

/**
 * 进入 AI 模式。
 * 生成 conversation_id，切换 DOM，调用 chat_prompt 发送首条消息。
 * 0.28.28：`selectionMaterial` 为可选划词引用材料（组合提问建议采纳）——
 * 首条消息 = 问题 + 材料在模型输入边界组合，气泡分体渲染。
 * @param {string} queryText 用户输入的查询文本
 * @param {string|null} [selectionMaterial] 划词引用材料快照
 */
export async function enterAiMode(queryText, selectionMaterial = null) {
    if (active) return;
    active = true;
    sessionGen++;
    conversationId = crypto.randomUUID();
    streamBuffer = "";
    awaitingConfirm = false;
    sendInFlight = false;
    pendingFollowup = null;
    lastTerminal = null;
    slotReady = false;
    currentQuestion = queryText;
    currentMaterial = selectionMaterial || null;
    renderPendingBar();

    // DOM 切换
    searchModeEl.hidden = true;
    aiModeEl.hidden = false;
    aiQueryEl.value = "";
    aiQueryEl.focus();

    // 0.24.4: 模式徽章统一走 ModeHeader（旧 .ai-mode-indicator 退役）
    modeHeader.set({id: "ai", icon: "sparkles", label: t("mode.ai.label")});

    // 清空搜索结果 + ghost/建议（避免 AI 模式下残留搜索导航提示与 Tab 建议条）
    results.clear();
    ghost.clear();
    suggestionBar.clear();

    // 加载当前模型标签（异步，不阻塞首条消息发送）
    refreshModelLabel();

    // 清空展示区
    aiContentEl.innerHTML = "";
    aiToolLineEl.innerHTML = "";
    // 新会话复位滚动跟随状态 + 隐藏遗留的"回到底部"按钮
    userScrolledUp = false;
    if (scrollBottomBtn) scrollBottomBtn.classList.remove("visible");
    syncWindowSize();

    // 发送首条消息（问题 + 材料在模型输入边界组合）
    await sendPrompt(composeMessageWithMaterial(queryText, currentMaterial), {
        question: queryText,
        material: currentMaterial,
    });
}

/**
 * 退出 AI 模式。
 * 中止活跃请求（如有），清空展示区，回到搜索模式。
 */
export function exitAiMode() {
    if (!active) return;

    // 中止活跃请求
    if (requestId !== null) {
        chatAbort(requestId).catch((e) =>
            console.warn("[ai-mode] chatAbort 失败:", e),
        );
        requestId = null;
    }
    // 0.28.28：启动中（request id 尚未返回）——递增代次使在途 chatPrompt 过期，
    // 受理成功后按旧代次追认取消（迟到请求不得残留）。
    if (sendInFlight) {
        sessionGen++;
        sendInFlight = false;
    }
    earlyStreamBuffer.clear();
    clearPendingFollowup();

    // 0.18.0: 清除看门狗 AI 活跃标志（即使 request 已 Done 也要清）
    clearMainAiActive().catch((e) =>
        console.warn("[ai-mode] clearMainAiActive 失败:", e),
    );

    // 清空状态
    active = false;
    conversationId = null;
    streamBuffer = "";
    awaitingConfirm = false;
    currentQuestion = "";
    currentMaterial = null;
    lastTerminal = null;
    if (throttle) {
        throttle.cancel();
        throttle = null;
    }
    streamRenderer = null;

    // DOM 切换回搜索模式
    aiModeEl.hidden = true;
    searchModeEl.hidden = false;
    // 0.24.4: 清 ModeHeader 徽章
    modeHeader.clear();
    aiContentEl.innerHTML = "";
    aiToolLineEl.innerHTML = "";
    aiRoundsEl.innerHTML = "";
    aiModelLabelEl.textContent = "";
    // 重置 footer 隐藏态（下次 enterAiMode 由 refreshModelLabel 控制）
    const footer = aiModelLabelEl.parentElement;
    if (footer) footer.classList.add("hidden");
    // 重建 #ai-content（被确认卡片替换后需要恢复）
    aiRoundsEl.appendChild(aiContentEl);

    // 搜索模式复位
    queryEl.value = "";
    queryEl.focus();
    search.reset();
    ghost.clear();
    // 0.18.0: 归零 maxHeight，让窗口从 AI 模式的高大高度收缩回搜索模式的紧凑高度
    resetMaxHeight();
    syncWindowSize();
}

// ── 发送消息 ──────────────────────────────────────────────────────────────────

/**
 * 在模型输入边界组合问题与引用材料（0.28.28）。
 * 材料以显式标注块随消息进入会话历史——后续追问通过历史理解材料，
 * 不重复附加原文；不使用附件围栏（围栏会被临时会话历史剥离）。
 */
function composeMessageWithMaterial(question, material) {
    if (!material) return question;
    return `${question}\n\n[引用材料]\n${material}\n[/引用材料]`;
}

/**
 * 调用 chat_prompt 发送消息。
 * 在 #ai-content 中先渲染用户消息气泡（问题 + 可展开引用材料），再创建 AI 回复区。
 * 0.28.28：发送代次校验——退出/取消后迟到的受理结果被追认取消，不复活展示。
 * @param {string} message 用户消息文本（已组合材料）
 * @param {{question?: string, material?: string|null}} [split] 分体渲染信息
 */
async function sendPrompt(message, split = null) {
    const gen = sessionGen;
    streamBuffer = "";
    awaitingConfirm = false;
    lastTerminal = null;
    slotReady = false;
    requestId = null;
    currentQuestion = split?.question ?? message;
    currentMaterial = split ? (split.material ?? null) : currentMaterial;
    sendInFlight = true;
    earlyStreamBuffer.begin(conversationId);

    // 构建当前轮 DOM：用户消息 + AI 回复区（含 typing 动画）
    aiContentEl.innerHTML = "";

    // 用户消息气泡：问题 + 可展开引用材料（分体渲染，问题始终可见）
    const userMsg = document.createElement("div");
    userMsg.className = "ai-user-message";
    const questionText = split?.question ?? message;
    userMsg.appendChild(document.createTextNode(questionText));
    if (currentMaterial) {
        userMsg.appendChild(buildMaterialExpander(currentMaterial));
    }
    aiContentEl.appendChild(userMsg);

    // AI 回复区
    const responseArea = document.createElement("div");
    responseArea.className = "ai-response-area";
    responseArea.innerHTML = renderTypingIndicator();
    aiContentEl.appendChild(responseArea);

    if (throttle) {
        throttle.cancel();
    }
    streamRenderer = null;

    // 滚动到最新内容
    scrollToBottom();
    // 新一轮开始（含 followup 折叠上一轮为单行摘要）用 resync 重新基准：
    // syncWindowSize 只增不减，会让窗口残留上一轮被折叠掉的高大高度
    resyncWindowSize();

    try {
        const rid = await chatPrompt(conversationId, message, {
            targetWindow: "main",
            ephemeral: true,
            thinkingEnabled: false,
        });
        if (gen !== sessionGen) {
            // 0.28.28：会话已切换/退出——追认取消迟到请求，不接入新会话
            chatAbort(rid).catch((e) =>
                console.warn("[ai-mode] 迟到请求追认取消失败:", e),
            );
            return;
        }
        requestId = rid;
        sendInFlight = false;
        for (const payload of earlyStreamBuffer.resolve(rid)) dispatchStreamPayload(payload);
    } catch (e) {
        sendInFlight = false;
        if (gen !== sessionGen || !active) return;  // 过期返回不渲染
        earlyStreamBuffer.clear();
        handlePromptError(e);
    }
}

/**
 * 输入框提交入口（0.28.28，keyboard.js Enter 调用）。
 *
 * - 空闲可续发（无启动中、无生成中、无确认卡）→ 直接发送；
 * - 启动中/生成中/等待确认 → 暂存一条待发消息（已有待发时不覆盖不重复入队，
 *   输入框内容保留）；本轮成功完成（Ready 槽位信号）后自动发送一次。
 */
export function submitInput() {
    if (!active) return;
    const text = aiQueryEl.value.trim();
    if (!text) return;
    const busy = sendInFlight || (requestId !== null && !slotReady) || awaitingConfirm;
    if (!busy) {
        aiQueryEl.value = "";
        void askFollowup(text, null);
        return;
    }
    if (pendingFollowup !== null) return;  // 已有待发：不覆盖、不重复入队
    pendingFollowup = text;
    aiQueryEl.value = "";
    renderPendingBar();
}

/**
 * 发送追问消息（0.17.6a；0.28.28 支持分体材料）。
 * 当前轮收进折叠摘要条后再发送新消息，保持同一 conversation_id。
 * @param {string} text 追问文本
 * @param {string|null} [material] 追问引用材料（追问默认不重复附加材料，
 *   由会话历史承载；显式传 null/字符串时按传值组合）
 */
export async function askFollowup(text, material = undefined) {
    if (!active || !text.trim()) return;
    // 当前轮有内容时收进折叠摘要条
    if (streamBuffer) {
        collapseToSummary(currentQuestion, streamBuffer);
    }
    const splitMaterial = material === undefined ? null : material;
    currentQuestion = text;
    currentMaterial = splitMaterial;
    await sendPrompt(
        composeMessageWithMaterial(text, splitMaterial),
        {question: text, material: splitMaterial},
    );
}

/**
 * 0.17.6a: 将当前临时对话提升为持久对话（Chord-Q）。
 *
 * AiMode 下 Alt+Q 触发：
 * 1. 调用后端 promote_ephemeral_conversation
 * 2. 后端 abort 当前请求 -> 导出临时消息 -> 写入 SQLite -> 清空临时记忆 -> 打开对话窗口
 * 3. 前端 exitAiMode → SearchMode
 */
export async function promoteToChat() {
    if (!active || !conversationId) return;

    // 如果有活跃请求且有内容，先 finalize（保留已有内容导出）
    if (streamBuffer && throttle) {
        throttle.cancel();
        if (streamRenderer) {
            streamRenderer.write(streamBuffer);
        }
    }

    try {
        await promoteEphemeralConversation(conversationId);
    } catch (e) {
        console.error("[ai-mode] promoteEphemeralConversation 失败:", e);
        // 即使失败也退出 AI 模式
    }

    // 退出 AI 模式回到搜索模式
    exitAiMode();
}

// ── 流式事件处理 ──────────────────────────────────────────────────────────────

/**
 * 处理 CHAT_STREAM 事件。
 * 按 request_id 过滤，分发 chunk.kind。
 */
function handleStreamEvent(event) {
    if (!active) return;
    const payload = event.payload;
    const {request_id, conversation_id} = payload;
    if (requestId === null && earlyStreamBuffer.capture(payload)) return;
    // 忽略不属于当前请求的 chunk
    if (request_id !== requestId) return;
    if (conversation_id !== conversationId) return;

    dispatchStreamPayload(payload);
}

function dispatchStreamPayload(payload) {
    const {chunk} = payload;

    switch (chunk.kind) {
        case "text":
            handleTextChunk(chunk.text);
            break;
        case "thinking":
            // 0.17.6 暂不展示 thinking（0.17.6a 可折叠展示）
            break;
        case "tool_call":
            handleToolCall(chunk.tool, chunk.arguments);
            break;
        case "tool_result":
            handleToolResult(chunk.call_id, chunk.success, chunk.summary);
            break;
        case "done":
            handleDone(chunk);
            break;
        case "error":
            handleError(chunk.message);
            break;
        case "max_turns_reached":
            handleMaxTurns(chunk.max_turns);
            break;
        // 0.28.28：后端请求槽已释放（可续发边界）——仅成功完成的 Ready
        // 自动发送待发项；失败/取消保留待发等待用户处理。
        case "ready":
            handleSlotReady();
            break;
    }
}

/**
 * 处理文本 chunk：追加到 buffer，rAF 节流渲染。
 */
function handleTextChunk(text) {
    streamBuffer += text;

    // 首次 text 到达时：移除 typing 动画，创建流式渲染器
    if (!streamRenderer) {
        const responseArea = aiContentEl.querySelector(".ai-response-area");
        if (!responseArea) return;
        responseArea.innerHTML = "";
        streamRenderer = renderMarkdownStream(responseArea);
        throttle = createThrottledRenderer((accumulated) => {
            if (streamRenderer) {
                streamRenderer.write(accumulated);
            }
            // 流式渲染期间同步窗口高度 + 滚动到底部
            scrollToBottom();
            syncWindowSize();
        });
    }

    throttle.schedule(streamBuffer);
}

/**
 * 处理 tool_call：更新工具行为单行描述。
 */
function handleToolCall(toolName, args) {
    aiToolLineEl.innerHTML = renderToolLine(toolName, args);
    scrollToBottom();
    syncWindowSize();
}

/**
 * 处理 tool_result：更新工具行为完成状态。
 * 注意：tool_result 只有 call_id，没有 tool name，
 * 保持工具行不变（tool_call 已设置名称），只追加结果摘要。
 */
function handleToolResult(callId, success, summary) {
    // 从工具行读取当前工具名
    const nameEl = aiToolLineEl.querySelector(".ai-tool-name");
    const toolName = nameEl ? nameEl.textContent : "";
    if (toolName) {
        aiToolLineEl.innerHTML = renderToolResultLine(toolName, success, summary);
    }
    scrollToBottom();
}

/**
 * 处理 done：完成流式渲染。
 */
function handleDone(chunk) {
    // 最终渲染（确保最后的内容已写入）
    if (throttle) {
        throttle.cancel();
        if (streamRenderer && streamBuffer) {
            streamRenderer.write(streamBuffer);
        }
    }

    // 如果有内容，注入复制按钮
    if (streamBuffer) {
        injectCopyButton();
    }

    // 清理状态（0.28.28：requestId 保留——Ready 槽位信号仍按该 id 过滤；
    // 可续发判定走 slotReady）
    if (throttle) {
        throttle = null;
    }
    streamRenderer = null;
    lastTerminal = "done";

    // 工具行延迟清空（让用户看到最后工具的结果）。
    // requestId/代次判空：防止过期定时器清掉 followup 新一轮的工具行。
    const gen = sessionGen;
    setTimeout(() => {
        if (!active || gen !== sessionGen || requestId !== null) return;
        aiToolLineEl.innerHTML = "";
        // 工具行消失后窗口可能缩短，用 resync 重新基准高度（syncWindowSize 只增不减）
        resyncWindowSize();
    }, 2000);

    scrollToBottom();
    syncWindowSize();
}

/**
 * 处理错误。
 *
 * 0.19.1：Error 是终态，用户接下来大概率想重试或退出——
 * 立即清除 MAIN_WINDOW_AI_ACTIVE，恢复看门狗失焦隐藏 + Alt+Space toggle。
 * 不靠后端 30s 兜底定时器（那 30s 内用户按 Alt+Space 会走 "AI 活跃" 分支
 * 只 refocus 不 toggle，感知为"窗口唤不起来了"）。
 */
function handleError(message) {
    if (throttle) {
        throttle.cancel();
        throttle = null;
    }
    streamRenderer = null;
    lastTerminal = "error";

    // 立即清除看门狗 AI 活跃标志——Error 是终态，不应继续阻止 toggle / 失焦隐藏
    clearMainAiActive().catch((e) =>
        console.warn("[ai-mode] clearMainAiActive 失败 (handleError):", e),
    );

    const responseArea = aiContentEl.querySelector(".ai-response-area");
    if (responseArea) {
        responseArea.innerHTML = `<div class="ai-error-message">${escapeHtml(message)}</div>`;
    }
    scrollToBottom();
    syncWindowSize();
}

/**
 * 处理最大轮次到达。
 *
 * 0.19.1：MaxTurnsReached 是终态，同 handleError——立即清除 AI 活跃标志。
 */
function handleMaxTurns(maxTurns) {
    lastTerminal = "max_turns";
    // 立即清除看门狗 AI 活跃标志——MaxTurnsReached 是终态
    clearMainAiActive().catch((e) =>
        console.warn("[ai-mode] clearMainAiActive 失败 (handleMaxTurns):", e),
    );

    const responseArea = aiContentEl.querySelector(".ai-response-area");
    if (responseArea) {
        responseArea.insertAdjacentHTML(
            "beforeend",
            `<div class="ai-max-turns-warning">${escapeHtml(t("ai.max_turns_warning", {max: maxTurns}))}</div>`,
        );
    }
    scrollToBottom();
    syncWindowSize();
}

/**
 * 处理 chat_prompt 调用错误。
 */
function handlePromptError(e) {
    const msg = String(e);
    // AlreadyActive 错误
    if (msg.startsWith("AlreadyActive:")) {
        const win = msg.slice("AlreadyActive:".length);
        setResponseError(escapeHtml(t("ai.already_active", {window: win})));
    } else {
        setResponseError(escapeHtml(msg));
    }
}

/** 在 AI 回复区显示错误信息。 */
function setResponseError(html) {
    const responseArea = aiContentEl.querySelector(".ai-response-area");
    if (responseArea) {
        responseArea.innerHTML = `<div class="ai-error-message">${html}</div>`;
    }
    scrollToBottom();
    syncWindowSize();
}

// ── 确认事件处理 ──────────────────────────────────────────────────────────────

/**
 * 处理 CHAT_CONFIRM_ACTION 事件。
 */
function handleConfirmEvent(event) {
    if (!active) return;
    const payload = event.payload;
    if (!payload) return;
    // 按 conversation_id 过滤
    if (payload.conversation_id && payload.conversation_id !== conversationId) return;

    awaitingConfirm = true;

    // 在 AI 回复区显示确认卡片
    const card = renderConfirmCard(payload, async (confirmId, approved) => {
        try {
            await confirmChatAction(confirmId, approved);
        } catch (e) {
            console.error("[ai-mode] confirmChatAction 失败:", e);
        }
        // 确认后移除卡片，恢复流式输出
        card.remove();
        awaitingConfirm = false;
        // 卡片是临时 UI，移除后窗口高度重新贴合实际内容（缩回被卡片撑开的高度）
        resyncWindowSize();
    });

    const responseArea = aiContentEl.querySelector(".ai-response-area");
    if (responseArea) {
        responseArea.innerHTML = "";
        responseArea.appendChild(card);
    }
    scrollToBottom();
    syncWindowSize();
}

// ── 待发消息条与引用材料（0.28.28）─────────────────────────────────────────

/**
 * 渲染待发消息条：可编辑输入 + 取消按钮。显示「上一条完成后发送」提示。
 * 至多一条待发；Enter 在条内仅确认编辑（blur），主输入框 Enter 遵循输入语义。
 */
function renderPendingBar() {
    const bar = document.getElementById("ai-pending-bar");
    const input = document.getElementById("ai-pending-input");
    if (!bar || !input) return;
    if (pendingFollowup === null) {
        bar.classList.add("hidden");
        input.value = "";
        return;
    }
    input.value = pendingFollowup;
    bar.classList.remove("hidden");
    if (!input.dataset.bound) {
        input.dataset.bound = "1";
        input.addEventListener("input", () => {
            pendingFollowup = input.value;
        });
        input.addEventListener("keydown", (e) => {
            if (e.key === "Enter" && !e.isComposing) {
                e.preventDefault();
                input.blur();
                aiQueryEl.focus();
            }
        });
        document.getElementById("ai-pending-cancel")?.addEventListener("click", () => {
            clearPendingFollowup();
            aiQueryEl.focus();
        });
    }
}

/** 清除待发消息（取消 / 已发送 / 会话退出）。 */
function clearPendingFollowup() {
    pendingFollowup = null;
    renderPendingBar();
}

/** 0.28.28：Ready（后端槽位已释放）处理——仅成功完成自动发送待发项。 */
function handleSlotReady() {
    if (!active) return;
    slotReady = true;
    const hasPending = pendingFollowup !== null;
    const succeeded = lastTerminal === "done";
    if (hasPending && succeeded && !awaitingConfirm) {
        const text = pendingFollowup;
        clearPendingFollowup();
        void askFollowup(text, null);
    }
}

/**
 * 构建当前轮用户气泡的引用材料展开区（<details>，原生展开语义）。
 * 超长材料明确提示可能超出上下文预算，不静默截断。
 */
function buildMaterialExpander(material) {
    const details = document.createElement("details");
    details.className = "ai-user-material";
    const summary = document.createElement("summary");
    const count = material.length;
    summary.textContent = count > ASK_AI_MATERIAL_WARN_CHARS
        ? t("ai.material.expander_long", {count})
        : t("ai.material.expander", {count});
    if (count > ASK_AI_MATERIAL_WARN_CHARS) {
        details.classList.add("ai-user-material--long");
    }
    const body = document.createElement("pre");
    body.className = "ai-user-material-body";
    body.textContent = material;
    details.appendChild(summary);
    details.appendChild(body);
    // 0.28.28：展开/收起后窗口高度贴合实际内容（内容展开必须撑开窗口）
    details.addEventListener("toggle", () => {
        resyncWindowSize();
        scrollToBottom();
    });
    return details;
}

/** 引用材料超长提示阈值（字符数；超出示意提示，不截断）。 */
const ASK_AI_MATERIAL_WARN_CHARS = 12000;

// ── 折叠摘要 ──────────────────────────────────────────────────────────────────

/**
 * 将当前轮收进折叠摘要条，插入 #ai-rounds 中 #ai-content 之前。
 * 摘要条默认折叠，点击展开后显示完整回答。
 * @param {string} question 用户问题文本
 * @param {string} answer assistant 回答 Markdown 文本
 */
function collapseToSummary(question, answer) {
    const summary = document.createElement("div");
    summary.className = "ai-round-summary";
    summary.dataset.collapsed = "";

    // 摘要头：用户问题截断 + 展开按钮
    const header = document.createElement("div");
    header.className = "ai-round-summary-header";

    const qText = document.createElement("span");
    qText.className = "ai-round-summary-q";
    const truncated = question.length > 60
        ? question.slice(0, 60) + "…"
        : question;
    qText.textContent = truncated;
    qText.title = question;

    const toggleBtn = document.createElement("button");
    toggleBtn.className = "ai-round-summary-toggle";
    toggleBtn.textContent = t("ai.round_expand");
    toggleBtn.addEventListener("click", () => {
        const isCollapsed = summary.hasAttribute("data-collapsed");
        if (isCollapsed) {
            summary.removeAttribute("data-collapsed");
            toggleBtn.textContent = t("ai.round_collapse");
        } else {
            summary.setAttribute("data-collapsed", "");
            toggleBtn.textContent = t("ai.round_expand");
        }
    });

    header.appendChild(qText);
    header.appendChild(toggleBtn);
    summary.appendChild(header);

    // 摘要体：完整回答（Cherry 渲染）
    const body = document.createElement("div");
    body.className = "ai-round-summary-body";
    // 0.17.6a: 用 renderMarkdown 一次性渲染（非流式，历史轮已完整）
    renderMarkdown(answer, {container: body});
    summary.appendChild(body);

    // 插入到 #ai-rounds 中 #ai-content 之前
    aiRoundsEl.insertBefore(summary, aiContentEl);
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/**
 * 在 AI 回复区底部注入复制按钮。
 */
function injectCopyButton() {
    const responseArea = aiContentEl.querySelector(".ai-response-area");
    if (!responseArea) return;
    const btn = document.createElement("button");
    btn.className = "ai-copy-btn";
    btn.textContent = t("ai.copy");
    btn.addEventListener("click", async () => {
        try {
            await copyToClipboard(streamBuffer);
            btn.textContent = t("ai.copied");
            setTimeout(() => {
                btn.textContent = t("ai.copy");
            }, 1500);
        } catch (e) {
            console.error("[ai-mode] 复制失败:", e);
        }
    });
    responseArea.appendChild(btn);
}

/**
 * 判断 #ai-display 是否在底部（允许 80px 阈值）。
 */
function isAtBottom() {
    if (!aiDisplayEl) return true;
    const threshold = 80;
    return aiDisplayEl.scrollHeight - aiDisplayEl.scrollTop - aiDisplayEl.clientHeight < threshold;
}

/** 创建「回到底部」浮动按钮（absolute 定位在 #ai-mode 上，不随 #ai-display 内容滚动） */
function ensureScrollBottomBtn() {
    if (scrollBottomBtn) return;
    scrollBottomBtn = document.createElement("button");
    scrollBottomBtn.className = "ai-scroll-bottom-btn";
    scrollBottomBtn.title = "回到底部";
    scrollBottomBtn.innerHTML = iconHTML("chevron-down");
    scrollBottomBtn.addEventListener("click", () => {
        forceScrollToBottom();
    });
    const aiMode = aiDisplayEl?.parentElement;
    if (aiMode) aiMode.appendChild(scrollBottomBtn);
}

/** 强制滚动到底部并重置用户上滚标记 */
function forceScrollToBottom() {
    userScrolledUp = false;
    if (scrollBottomBtn) scrollBottomBtn.classList.remove("visible");
    if (aiDisplayEl) {
        aiDisplayEl.scrollTop = aiDisplayEl.scrollHeight;
    }
}

/** 绑定 scroll 事件监听用户手动滚动 */
function bindScrollListener() {
    if (!aiDisplayEl) return;
    aiDisplayEl.addEventListener("scroll", () => {
        if (isAtBottom()) {
            userScrolledUp = false;
            if (scrollBottomBtn) scrollBottomBtn.classList.remove("visible");
        } else {
            userScrolledUp = true;
            ensureScrollBottomBtn();
            if (scrollBottomBtn) scrollBottomBtn.classList.add("visible");
        }
    });
}

/**
 * 滚动 #ai-display 到底部，确保最新内容可见。
 * 用户手动上滚浏览历史时不夺滚动权（与对话窗口一致）。
 */
function scrollToBottom() {
    if (!aiDisplayEl) return;
    if (userScrolledUp) return;
    aiDisplayEl.scrollTop = aiDisplayEl.scrollHeight;
}

/**
 * 异步拉取当前 Ephemeral 模型并更新底部标签。
 * 失败静默降级（隐藏标签栏）。
 */
async function refreshModelLabel() {
    const footer = aiModelLabelEl.parentElement;
    try {
        const models = await getEphemeralModels();
        if (!models || !models.length) {
            aiModelLabelEl.textContent = "";
            if (footer) footer.classList.add("hidden");
            return;
        }
        const selected = models.find((m) => m.is_selected);
        if (selected) {
            aiModelLabelEl.textContent = selected.model_name;
            if (footer) footer.classList.remove("hidden");
        } else {
            aiModelLabelEl.textContent = "";
            if (footer) footer.classList.add("hidden");
        }
    } catch (e) {
        console.warn("[ai-mode] 获取模型列表失败:", e);
        aiModelLabelEl.textContent = "";
        if (footer) footer.classList.add("hidden");
    }
}

/** HTML 特殊字符转义。 */
function escapeHtml(text) {
    const div = document.createElement("div");
    div.textContent = String(text ?? "");
    return div.innerHTML;
}
