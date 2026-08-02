//! 长截图会话的唯一状态所有者。
//!
//! `ss` 上保留同名访问器作为迁移期兼容门面；真实数据、代际和在途任务都集中
//! 在 ScrollCaptureSession，避免 index / output / scroll 各自维护一份清理清单。

const SESSION_DEFAULTS = Object.freeze({
  scrollCapturePhase: 'idle',
  scrollFrames: null,
  scrollDirection: 'vertical',
  autoScroll: false,
  scrollHwnd: null,
  scrollBandW: 0,
  scrollBandH: 0,
  scrollBandX: 0,
  scrollBandY: 0,
  scrollTargetX: 0,
  scrollTargetY: 0,
  scrollSourceRect: null,
  scrollLastFrame: null,
  scrollKeyframes: null,
  scrollTrackingState: 'tracking',
  scrollLostFrameCount: 0,
  scrollCurrentTop: 0,
  scrollLastAcceptedShift: 0,
  scrollWheelStartedAtMs: null,
  scrollPendingDirection: 0,
  scrollUnchangedCount: 0,
  scrollLastDecision: null,
  scrollDecisions: null,
  scrollReplayFrames: null,
  scrollReplayBytes: 0,
  scrollPendingJump: null,
  _scrollCapturing: false,
  _scrollCaptureTimer: 0,
});

export const SCROLL_SESSION_KEYS = Object.freeze(Object.keys(SESSION_DEFAULTS));

export class ScrollCaptureSession {
  constructor() {
    this.captureGeneration = 0;
    this.manualWheelVersion = 0;
    this.autoWheelDelta = -120;
    this.autoLowConfidenceCount = 0;
    this.wheelForwardPending = false;
    this.queuedManualWheel = null;
    this.captureInFlight = null;
    this.captureFinalizing = false;
    this.exitHandler = null;
    this.reset();
  }

  get active() {
    return this.scrollCapturePhase !== 'idle' || this.autoScroll;
  }

  invalidate() {
    this.captureGeneration++;
    return this.captureGeneration;
  }

  exit(restoreSelection = true) {
    return this.exitHandler?.(restoreSelection);
  }

  reset() {
    this.invalidate();
    if (this._scrollCaptureTimer) clearTimeout(this._scrollCaptureTimer);
    for (const [key, value] of Object.entries(SESSION_DEFAULTS)) {
      this[key] = value === null && [
        'scrollFrames', 'scrollKeyframes', 'scrollDecisions', 'scrollReplayFrames',
      ].includes(key)
        ? []
        : value;
    }
    this.manualWheelVersion = 0;
    this.autoWheelDelta = -120;
    this.autoLowConfidenceCount = 0;
    this.queuedManualWheel = null;
    this.captureFinalizing = false;
  }
}

/** 把旧的 `ss.scroll*` 访问收敛成 session 属性，不要求所有调用方同一批迁移。 */
export function attachScrollSessionFacade(sharedState) {
  const session = new ScrollCaptureSession();
  sharedState.scrollSession = session;
  for (const key of SCROLL_SESSION_KEYS) {
    delete sharedState[key];
    Object.defineProperty(sharedState, key, {
      configurable: false,
      enumerable: true,
      get: () => session[key],
      set: (value) => { session[key] = value; },
    });
  }
  return session;
}
