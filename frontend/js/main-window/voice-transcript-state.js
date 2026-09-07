//! 语音转写 UI 纯状态模块（0.22.15 WP5）。
//!
//! 从 lifecycle.js 的事件接线中提取纯状态逻辑，使 Node 测试可覆盖：
//! - begin → preview-only → non-empty final → end
//! - begin → non-empty preview → empty partial → end
//! - begin → preview → empty final/error → fallback/base policy
//! - begin epoch 2 后收到 epoch 1 partial/final/end
//! - confirmed + preview 多句增长
//! - final 重复到达时幂等
//! - 原 query 非空时空录音不丢原 query
//!
//! 状态机由 recording epoch 驱动——旧 epoch 的事件全部 no-op。
//! 空结果（空 partial / 空 final）不是"清空"命令。

/**
 * @typedef {Object} VoiceTranscriptState
 * @property {number} epoch - 当前录音 epoch（0 = 无活跃录音）
 * @property {string} baseQuery - 录音开始前的 query 原文
 * @property {string} confirmed - 已定稿文本
 * @property {string} preview - 预览文本
 * @property {boolean} finalDelivered - 终稿是否已交付
 * @property {string|null} finalText - 终稿文本（交付后非 null）
 */

/**
 * 创建初始状态。
 * @returns {VoiceTranscriptState}
 */
export function createVoiceTranscriptState() {
    return {
        epoch: 0,
        baseQuery: "",
        confirmed: "",
        preview: "",
        finalDelivered: false,
        finalText: null,
    };
}

/**
 * 录音开始：保存 baseQuery，设置 epoch，清空 confirmed/preview。
 *
 * @param {VoiceTranscriptState} state
 * @param {string} baseQuery - 录音前的 query 原文
 * @param {number} [epoch] - 后端传入的 recording epoch（可选，不传则自增）
 * @returns {VoiceTranscriptState} 新状态
 */
export function beginRecording(state, baseQuery, epoch) {
    return {
        ...state,
        epoch: epoch != null ? epoch : state.epoch + 1,
        baseQuery: baseQuery || "",
        confirmed: "",
        preview: "",
        finalDelivered: false,
        finalText: null,
    };
}

/**
 * 处理 partial 事件。
 *
 * 不变量：
 * - 空 partial 是 no-op（不清空 confirmed/preview）
 * - 旧 epoch 的 partial 被丢弃
 * - 终稿已交付后 partial 被丢弃
 *
 * @param {VoiceTranscriptState} state
 * @param {Object} payload - {confirmed?, preview?, target?, epoch?}
 * @returns {VoiceTranscriptState} 新状态
 */
export function applyPartial(state, payload = {}) {
    // 终稿已交付，丢弃后续 partial
    if (state.finalDelivered) {
        return state;
    }

    // epoch 校验（payload.epoch 不存在时不校验，兼容旧格式）
    if (payload.epoch != null && payload.epoch !== state.epoch) {
        return state;
    }

    const confirmed = payload.confirmed ?? "";
    const preview = payload.preview ?? "";

    // 空结果不是清空命令
    if (!confirmed && !preview) {
        return state;
    }

    // confirmed 只追加：不允许回到更短的旧值
    // 如果新 confirmed 比 state.confirmed 短，说明是乱序事件，丢弃 confirmed 部分
    let newConfirmed = state.confirmed;
    if (confirmed && confirmed.length >= state.confirmed.length) {
        newConfirmed = confirmed;
    }

    return {
        ...state,
        confirmed: newConfirmed,
        preview: preview || state.preview, // 空 preview 不清空已有 preview
    };
}

/**
 * 处理 final 事件。
 *
 * 不变量：
 * - 终稿只交付一次（幂等）
 * - 空 final 不清空已有 confirmed/preview
 * - 旧 epoch 的 final 被丢弃
 *
 * @param {VoiceTranscriptState} state
 * @param {Object} payload - {text?, epoch?}
 * @returns {VoiceTranscriptState} 新状态
 */
export function applyFinal(state, payload = {}) {
    // 幂等：终稿已交付
    if (state.finalDelivered) {
        return state;
    }

    // epoch 校验
    if (payload.epoch != null && payload.epoch !== state.epoch) {
        return state;
    }

    const text = payload.text ?? "";

    // 空 final 不是清空命令
    if (!text) {
        return state;
    }

    return {
        ...state,
        finalDelivered: true,
        finalText: text,
    };
}

/**
 * 录音结束：清理录音态，保留终稿结果。
 *
 * @param {VoiceTranscriptState} state
 * @returns {VoiceTranscriptState} 新状态
 */
export function endRecording(state) {
    return {
        ...state,
        preview: "",
        // confirmed 保留（如果终稿为空但有 confirmed，仍可用于搜索）
        // finalDelivered/finalText 保留——调用方可用 getFinalText() 取
    };
}

/**
 * 取最终应填入 #query 的文本。
 *
 * - 终稿非空 → finalText
 * - 终稿为空但有 confirmed → confirmed
 * - 终稿为空且有 preview → preview（兜底）
 * - 以上都空 → baseQuery（恢复录音前状态）
 *
 * @param {VoiceTranscriptState} state
 * @returns {string}
 */
export function getFinalQuery(state) {
    if (state.finalText) {
        return state.finalText;
    }
    if (state.confirmed) {
        return state.confirmed;
    }
    if (state.preview) {
        return state.preview;
    }
    return state.baseQuery;
}

/**
 * 录音是否产生了非空结果。
 * @param {VoiceTranscriptState} state
 * @returns {boolean}
 */
export function hasResult(state) {
    return !!(state.finalText || state.confirmed || state.preview);
}
