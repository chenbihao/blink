//! 听写（hold-to-talk 语音输入）会话控制器（0.25.13）。
//!
//! 从 lifecycle.js 接管主窗语音事件接线，并按「听写投递端点（sink）」契约分发：
//! 后端 VOICE_* 事件带 target 标签（"g1" 搜索框 / "ai" AI 追问框），本模块只做
//! 会话状态与指示器渲染，文本落位交给对应输入面注册的 sink——事件接线只写一次，
//! 新增输入面（或后端新增 target）无需再改本模块。
//!
//! 职责边界：
//! - 本模块：voiceState（voice-transcript-state）、voice-active/voice-error body 类、
//!   语音指示器生命周期（显示/标签/波形/避让带）、voice-error 3s 定时器、事件分发
//! - sink（各输入面自注册）：partial/final 文本落位、ghost 联动、恢复基准文本
//!
//! 终稿通道：VOICE_FINAL {text, target}（0.25.13 起接替 chord 时代命名的
//! chord-fill-query 裸字符串）。"填入后是否触发搜索/发送"由 sink 自决。

import {listen} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {
    applyFinal,
    applyPartial,
    beginRecording,
    createVoiceTranscriptState,
    endRecording,
    getFinalQuery,
    hasResult,
} from "./voice-transcript-state.js";
import {t} from "../i18n/index.js";

/**
 * @typedef {Object} DictationSink
 * @property {() => string} currentText 录音开始时的基准文本（空录音/取消时恢复用）
 * @property {() => HTMLElement|null} indicatorEl 本输入面的语音指示器（.voice-indicator）
 * @property {(ctx: {epoch: number, baseText: string}) => void} begin 录音开始（冻结 overlay 等前置）
 * @property {(view: {confirmed?: string, preview?: string, text?: string}) => void} partial
 *     流式文本落位（confirmed/preview 双字段或旧格式 text）
 * @property {(text: string) => void} final 终稿落位（是否触发搜索/提交由 sink 自决）
 * @property {(baseText: string) => void} restore 空录音/取消——恢复基准文本
 * @property {(text: string) => void} settle 兜底落位（confirmed-only 路径，文本一致时 no-op）
 * @property {() => void} end 录音收尾（解冻 overlay、清除 preview 样式等）
 */

/** @type {Map<string, DictationSink>} target 标签 → sink */
const sinks = new Map();

/** @type {string|null} 当前录音的 target 标签（null = 无活跃录音） */
let activeTarget = null;

/** @type {DictationSink|null} 当前录音的 sink */
let activeSink = null;

/** voice-error 的 3s 自动隐藏定时器 ID。 */
let voiceErrorTimer = null;

/** 0.22.15: 语音转写纯状态模块——事件接线只做分发 */
let voiceState = createVoiceTranscriptState();

/** 注册输入面的听写 sink。 @param {string} target 后端 VoiceTarget 标签 @param {DictationSink} sink */
export function registerSink(target, sink) {
    sinks.set(target, sink);
}

/** 当前是否有活跃录音。 */
export function isRecording() {
    return activeTarget !== null;
}

/** 清除 voice-error 状态：取消定时器 + 移除 body 类。 */
function clearVoiceError() {
    if (voiceErrorTimer) {
        clearTimeout(voiceErrorTimer);
        voiceErrorTimer = null;
    }
    document.body.classList.remove("voice-error");
}

/** 重置指示器到默认态（隐藏 + 波形复位 + 默认标签）。 @param {HTMLElement|null} vi */
function resetIndicator(vi) {
    if (!vi) return;
    vi.classList.add("hidden");
    vi.querySelectorAll(".vw-bar").forEach((bar) => (bar.style.height = "4px"));
    vi.querySelector(".voice-wave")?.classList.remove("voice-loading", "voice-error");
    const label = vi.querySelector(".voice-label");
    if (label) label.textContent = t("voice.indicator.recording");
}

/**
 * 按指示器实测宽度写入避让变量——指示器盖在输入框右端上，
 * CSS 在 body.voice-active 下把（指示器宽 + 间距）加进输入框 padding-right，
 * partial 追加文本末尾自然停在指示器左侧（0.25.12 机制，各输入面共用）。
 * @param {HTMLElement|null} vi
 */
function syncAvoidWidth(vi) {
    if (!vi) return;
    document.body.style.setProperty(
        "--voice-avoid-w",
        (vi.offsetWidth + 8) + "px" // 8 ≈ --space-sm 间距
    );
}

/** 注册事件监听（main.js 装配期调用一次）。 */
export function init() {
    // 录音开始 → 激活对应 sink + 显示语音指示器
    listen(EVENTS.VOICE_RECORDING_START, (event) => {
        const {target, epoch} = event.payload ?? {};
        const sink = sinks.get(target);
        if (!sink) {
            console.warn(`[dictation] 无 target=${target} 的听写 sink，忽略录音事件`);
            return;
        }
        clearVoiceError();
        // 初始化语音转写状态，保存 baseQuery（空录音/失败/取消时恢复）；
        // 使用后端传入的 epoch，使前端 epoch 与后端 epoch 同步
        voiceState = beginRecording(voiceState, sink.currentText(), epoch ?? 0);
        activeTarget = target;
        activeSink = sink;
        document.body.classList.add("voice-active");
        sink.begin({epoch: epoch ?? 0, baseText: voiceState.baseQuery});
        const vi = sink.indicatorEl();
        if (vi) {
            vi.classList.remove("hidden");
            // 录音开始：波形切回绿色（移除加载态蓝色 + 错误态红色）
            vi.querySelector(".voice-wave")?.classList.remove("voice-loading", "voice-error");
            syncAvoidWidth(vi);
        }
    });

    // 语音状态提示（模型加载中等，非错误性质）。
    // 注意：不设 voice-active——只有真正录音（voice-recording-start）才设，
    // 避免模型加载中隐藏 Chord 提示。
    listen(EVENTS.VOICE_STATUS, (event) => {
        const {message, target} = event.payload ?? {};
        if (target !== activeTarget || !message) return;
        const vi = activeSink?.indicatorEl();
        if (vi) {
            vi.classList.remove("hidden");
            const label = vi.querySelector(".voice-label");
            if (label) label.textContent = message;
            // 标签文案变化会改变指示器宽度，同步刷新避让变量
            syncAvoidWidth(vi);
            // 模型加载中：波形转蓝色（清除可能残留的错误态红色）
            vi.querySelector(".voice-wave")?.classList.remove("voice-error");
            vi.querySelector(".voice-wave")?.classList.add("voice-loading");
        }
    });

    // 流式 partial 文字实时更新当前输入面。
    // 录音期间不 dispatch input——伪流式引擎空 confirmed 阶段 dispatch 会触发
    // 无意义搜索（每个音频 chunk 一次）；录音结束后由 VOICE_FINAL 落位。
    listen(EVENTS.VOICE_PARTIAL, (event) => {
        const payload = event.payload ?? {};
        if (payload.target !== activeTarget || !activeSink) return;

        // 通过纯状态模块处理，空 partial 是 no-op
        voiceState = applyPartial(voiceState, payload);
        const {confirmed, preview} = voiceState;

        if (confirmed || preview) {
            // confirmed 填入输入框（已定稿文本），preview 走 sink 的预览通道
            activeSink.partial({confirmed, preview});
        } else if (payload.text) {
            // 兼容旧格式（真流式 / 非流式引擎）
            activeSink.partial({text: payload.text});
        }
    });

    // 录音音量波动条
    listen(EVENTS.VOICE_LEVEL, (event) => {
        const {level, target} = event.payload ?? {};
        if (target !== activeTarget) return;
        const vi = activeSink?.indicatorEl();
        vi?.classList.remove("hidden");
        const lv = Math.max(0, Math.min(1, level || 0));
        vi?.querySelectorAll(".vw-bar").forEach((bar, i) => {
            const factor = [0.6, 0.85, 1.0, 0.85, 0.6][i] || 0.7;
            // jitter 独立于 lv：即使安静时也有微妙呼吸感
            const jitter = (Math.sin(Date.now() / 80 + i * 1.3) + 1) * 0.08;
            bar.style.height = Math.max(4, (lv * factor + jitter) * 20) + "px";
        });
    });

    // 终稿统一交付通道（G1/AI）。是否触发搜索/提交由 sink 自决。
    listen(EVENTS.VOICE_FINAL, (event) => {
        const {text, target} = event.payload ?? {};
        if (target !== activeTarget || !text) return;
        // 先通过状态模块标记终稿已交付，使 hasResult() 返回 true，
        // 防止 VOICE_RECORDING_END 用 baseQuery 覆盖已填入的终稿文本
        voiceState = applyFinal(voiceState, {text});
        activeSink?.final(text);
    });

    // 录音结束 → 隐藏指示器 + 恢复/兜底落位 + 通知 sink 收尾。
    // 同时清除 voice-error 状态——松键后 voice-active 和 voice-error 都应立即移除，
    // 让 chord 提示能立刻恢复显示。
    listen(EVENTS.VOICE_RECORDING_END, (event) => {
        const {target} = event.payload ?? {};
        if (target != null && target !== activeTarget) return;
        const sink = activeSink;
        clearVoiceError();
        document.body.classList.remove("voice-active");
        voiceState = endRecording(voiceState);
        if (sink) {
            // 终稿为空且无 confirmed/preview → 恢复 baseQuery（空录音/失败/取消）
            if (!hasResult(voiceState)) {
                sink.restore(voiceState.baseQuery);
            } else {
                // 有结果但 VOICE_FINAL 未到达（如 confirmed-only 路径），
                // 用 getFinalQuery 兜底落位（文本一致时 sink 内 no-op）
                sink.settle(getFinalQuery(voiceState));
            }
            sink.end();
        }
        resetIndicator(sink?.indicatorEl());
        activeTarget = null;
        activeSink = null;
    });

    // 语音错误提示（STT 未配置 / 服务未启动等）。
    // 设计铁则：所有语音状态统一在波形动画区域展示——
    // 绿色=录音中 · 蓝色=加载中 · 红色=错误。
    listen(EVENTS.VOICE_ERROR, (event) => {
        const {message, target} = event.payload ?? {};
        // 已有活跃录音时只接受当前 target 的错误；无录音时接受任意已知 target
        // （录音启动失败时 START 可能未发出，activeTarget 为 null）
        if (activeTarget != null && target !== activeTarget) return;
        const sink = sinks.get(target) ?? activeSink;
        if (!sink || !message) return;
        document.body.classList.remove("voice-active");
        // 添加 voice-error 标记——隐藏 chord 提示，避免错误文案与 chord 键帽重叠
        document.body.classList.add("voice-error");
        sink.end(); // 确保解冻（错误可能发生在录音中）
        const vi = sink.indicatorEl();
        if (vi) {
            vi.classList.remove("hidden");
            const label = vi.querySelector(".voice-label");
            if (label) label.textContent = message;
            const wave = vi.querySelector(".voice-wave");
            if (wave) {
                wave.classList.remove("voice-loading"); // 清除可能残留的加载态
                wave.classList.add("voice-error");
            }
        }
        // 3s 后隐藏指示器 + 恢复默认文案 + 移除 voice-error 标记。
        // 若期间收到 RECORDING_START / RECORDING_END，clearVoiceError 会取消此定时器
        voiceErrorTimer = setTimeout(() => {
            voiceErrorTimer = null;
            document.body.classList.remove("voice-error");
            resetIndicator(sinks.get(target)?.indicatorEl());
        }, 3000);
    });
}

/** 窗口重新唤起（SHOWN）时的防御性清理：清残留录音态，指示器全部复位。 */
export function resetOnShown() {
    clearVoiceError();
    document.body.classList.remove("voice-active");
    document.querySelectorAll(".voice-indicator").forEach((vi) => resetIndicator(vi));
    voiceState = createVoiceTranscriptState();
    activeTarget = null;
    activeSink = null;
}
