//! 输入状态核心纯逻辑。
//!
//! 与 DOM / Tauri 无关的 UI revision / view epoch / context 去重逻辑。
//! 由 `input-state.js` 消费，由 `input-state-core.test.mjs` 测试。

/**
 * @typedef {Object} InputUiState
 * @property {number} revision
 * @property {boolean} altDown
 * @property {boolean} windowVisible
 * @property {boolean} exclusiveChordActive
 */

/**
 * @typedef {Object} ViewContext
 * @property {number} viewEpoch
 * @property {number} revision
 * @property {boolean} queryEmpty
 * @property {boolean} aiMode
 */

/**
 * 创建初始输入状态管理器。
 *
 * 状态管理器负责：
 * - 跟踪当前 view epoch + revision
 * - 跟踪当前 queryEmpty / aiMode
 * - 接收后端 InputUiState 事件/快照，以 revision 去重
 * - 判定 context 变化是否需要上报后端
 *
 * @returns {InputStateCore}
 */
export function createInputStateCore() {
  /** @type {number} */ let viewEpoch = 0;
  /** @type {number} */ let contextRevision = 0;
  /** @type {boolean} */ let queryEmpty = true;
  /** @type {boolean} */ let aiMode = false;
  /** @type {number} */ let lastAppliedRevision = 0;
  /** @type {InputUiState|null} */ let currentState = null;

  return {
    /** 当前 view epoch（0 = 未注册）。 */
    get viewEpoch() { return viewEpoch; },
    /** 当前 query 是否为空。 */
    get queryEmpty() { return queryEmpty; },
    /** 当前 AI 模式。 */
    get aiMode() { return aiMode; },
    /** 最近接受的 UI 状态（null = 尚未收到）。 */
    get state() { return currentState; },

    /**
     * 设置 view epoch（register 成功后调）。
     * @param {number} epoch
     */
    setViewEpoch(epoch) {
      viewEpoch = epoch;
      contextRevision = 0;
    },

    /**
     * 尝试接受一个后端 UI 状态事件/快照。
     *
     * 以 revision 去重：只接受比当前 lastAppliedRevision 更大的 revision。
     * 相同或更小的 revision 被拒绝（旧状态/重复事件）。
     *
     * @param {InputUiState} state
     * @returns {boolean} 是否接受（true = 状态已更新，false = 被丢弃）
     */
    applyState(state) {
      if (!state || typeof state.revision !== "number") return false;
      // 首次：接受任何 revision（含 0）
      if (currentState === null) {
        currentState = state;
        lastAppliedRevision = state.revision;
        return true;
      }
      // 后续：只接受更大的 revision
      if (state.revision > lastAppliedRevision) {
        currentState = state;
        lastAppliedRevision = state.revision;
        return true;
      }
      return false;
    },

    /**
     * 尝试更新视图上下文（queryEmpty / aiMode 变化时调）。
     *
     * 只在实际变化时返回需要上报的新 context；未变化返回 null。
     *
     * @param {boolean} newQueryEmpty
     * @param {boolean} newAiMode
     * @returns {ViewContext|null} 需要上报的 context（或 null 表示无变化）
     */
    updateContext(newQueryEmpty, newAiMode) {
      if (newQueryEmpty === queryEmpty && newAiMode === aiMode) {
        return null;
      }
      queryEmpty = newQueryEmpty;
      aiMode = newAiMode;
      contextRevision += 1;
      return {
        viewEpoch,
        revision: contextRevision,
        queryEmpty,
        aiMode,
      };
    },

    /**
     * WebView reload/recreate 时重置 view epoch。
     * 旧 epoch 的 context update 后端会丢弃。
     */
    reset() {
      viewEpoch = 0;
      contextRevision = 0;
      queryEmpty = true;
      aiMode = false;
      lastAppliedRevision = 0;
      currentState = null;
    },
  };
}

/**
 * @typedef {Object} InputStateCore
 * @property {() => number} viewEpoch
 * @property {() => boolean} queryEmpty
 * @property {() => boolean} aiMode
 * @property {() => InputUiState|null} state
 * @property {(epoch: number) => void} setViewEpoch
 * @property {(state: InputUiState) => boolean} applyState
 * @property {(newQueryEmpty: boolean, newAiMode: boolean) => ViewContext|null} updateContext
 * @property {() => void} reset
 */
