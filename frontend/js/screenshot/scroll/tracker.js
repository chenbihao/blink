//! 长截图单帧追踪决策。纯算法层，不依赖 DOM / Tauri / 全局 session。

import {
  createGrayFingerprint, createVerticalReference,
  estimateVerticalShift, planPositionedIncrement, positionedFrameBounds,
  relocalizeFromKeyframes, relocalizeFromPositionedContent,
} from './stitch.js';

export const SCROLL_DECISION_SCHEMA_VERSION = 1;
export const MAX_SCROLL_KEYFRAMES = 64;

/** 生产采集与离线回放共用同一套关键帧保留策略。 */
export function rememberScrollKeyframe(keyframes, frame, top) {
  if (keyframes.some((item) => Math.abs(item.top - top) <= 3)) return keyframes;
  const probe = createGrayFingerprint(frame);
  const reference = createVerticalReference(frame);
  if (!probe || !reference) return keyframes;
  const next = [...keyframes, { top, probe, reference }];
  if (next.length <= MAX_SCROLL_KEYFRAMES) return next;

  // 按空间位置均匀抽样；滚动历史可能反复回访局部，不能按插入顺序裁剪。
  const ordered = next.sort((a, b) => a.top - b.top);
  const thinned = [];
  for (let index = 0; index < MAX_SCROLL_KEYFRAMES; index++) {
    const sourceIndex = Math.round(index * (ordered.length - 1) / (MAX_SCROLL_KEYFRAMES - 1));
    const keyframe = ordered[sourceIndex];
    if (thinned.at(-1) !== keyframe) thinned.push(keyframe);
  }
  return thinned;
}

function finiteOrNull(value) {
  return Number.isFinite(value) ? value : null;
}

function matchConfidence(match, frameHeight) {
  if (!match) return 0;
  if (match.status === 'unchanged') return Math.max(0, 1 - match.score / 2.5);
  if (match.status !== 'matched') return 0;
  const scoreConfidence = Math.max(0, 1 - match.score / 22);
  const overlap = Math.max(0, 1 - Math.abs(match.shift) / Math.max(1, frameHeight));
  return Math.round(Math.min(1, scoreConfidence * 0.75 + overlap * 0.25) * 1000) / 1000;
}

function rejectionReason(match, motionTimedOut) {
  if (match?.reason === 'ambiguous') return 'ambiguous';
  if (match?.status === 'unchanged') return 'unchanged';
  if (motionTimedOut) return 'motion-timeout';
  if (match?.reason === 'tracking-lost') return 'no-overlap';
  return match?.reason || 'no-overlap';
}

function makeDecision(fields) {
  return {
    schemaVersion: SCROLL_DECISION_SCHEMA_VERSION,
    accepted: false,
    reason: 'low-confidence',
    source: 'none',
    expectedDirection: 0,
    candidateTop: null,
    previousTop: null,
    positionDelta: 0,
    bestScore: null,
    secondScore: null,
    overlapRatio: 0,
    confidence: 0,
    appendRange: null,
    motionTimedOut: false,
    confirmation: 'none',
    ...fields,
  };
}

/**
 * 根据一张完整采集帧计算唯一决策，不修改输入状态。
 * state: { frames, keyframes, lastFrame, currentTop, trackingState }
 */
export function trackScrollFrame(state, frame, options = {}) {
  const expectedDirection = Math.sign(options.expectedDirection ?? 0);
  const motionTimedOut = options.motionTimedOut === true;
  if (!state.lastFrame) {
    const placement = { edge: 'first', startRow: 0, rowCount: frame.height, targetTop: 0 };
    return {
      placement,
      nextTop: 0,
      match: null,
      relocalized: null,
      pendingJump: null,
      decision: makeDecision({
        accepted: true,
        reason: 'first-frame',
        source: 'initial',
        expectedDirection,
        candidateTop: 0,
        previousTop: null,
        overlapRatio: 1,
        confidence: 1,
        appendRange: { top: 0, bottom: frame.height },
        motionTimedOut,
      }),
    };
  }

  const wasLost = state.trackingState === 'lost';
  const lostFrameCount = Math.max(0, state.lostFrameCount ?? (wasLost ? 1 : 0));
  const recoveryDistanceLimit = frame.height * 0.75 * Math.min(8, lostFrameCount + 1);
  const rejectImplausibleRecovery = (candidate) => {
    if (!candidate || Math.abs(candidate.top - state.currentTop) <= recoveryDistanceLimit) {
      return candidate;
    }
    match = {
      ...candidate.match,
      status: 'no-match',
      reason: 'recovery-distance',
      shift: 0,
      candidateShift: candidate.top - state.currentTop,
    };
    return null;
  };
  let match = wasLost
    ? { status: 'no-match', shift: 0, score: Infinity, reason: 'tracking-lost' }
    : estimateVerticalShift(state.lastFrame, frame, {
      expectedDirection,
      strictDirection: expectedDirection !== 0,
      rejectAmbiguous: true,
    });
  let nextTop = state.currentTop + match.shift;
  let relocalized = null;
  let attemptedRelocalization = false;
  if (match.status === 'no-match') {
    attemptedRelocalization = true;
    relocalized = relocalizeFromKeyframes(
      state.frames,
      state.keyframes,
      frame,
      state.currentTop,
      expectedDirection,
      { trackingLost: wasLost },
    );
    relocalized = rejectImplausibleRecovery(relocalized);
    if (relocalized) {
      match = relocalized.match;
      nextTop = relocalized.top;
    }
  }
  if (!relocalized && attemptedRelocalization) {
    relocalized = relocalizeFromPositionedContent(
      state.frames,
      frame,
      state.currentTop,
      expectedDirection,
      { trackingLost: wasLost },
    );
    relocalized = rejectImplausibleRecovery(relocalized);
    if (relocalized) {
      match = relocalized.match;
      nextTop = relocalized.top;
    }
  }

  const accepted = (match.status === 'matched' || match.status === 'unchanged')
    && (relocalized || match.shift !== 0);
  const source = relocalized
    ? (relocalized.scope === 'content' ? 'content-partition' : `keyframe-${relocalized.scope}`)
    : (attemptedRelocalization ? (wasLost ? 'keyframe-search' : 'adjacent+keyframe') : 'adjacent');
  const rejectedCandidateTop = Number.isFinite(match.candidateShift)
    ? Math.round(state.currentTop + match.candidateShift)
    : null;
  const common = {
    expectedDirection,
    source,
    candidateTop: accepted ? Math.round(nextTop) : rejectedCandidateTop,
    previousTop: state.currentTop,
    positionDelta: accepted ? Math.round(nextTop - state.currentTop) : 0,
    bestScore: finiteOrNull(match.score),
    secondScore: finiteOrNull(match.secondScore ?? match.rivalScore),
    overlapRatio: accepted
      ? Math.round(Math.max(0, 1 - Math.abs(match.shift) / frame.height) * 1000) / 1000
      : 0,
    confidence: matchConfidence(match, frame.height),
    motionTimedOut,
  };
  // 重定位没有相邻帧的连续性兜底；低于此底线时二次看到同一张嵌套截图
  // 也不能证明它是页面真实位置，因此直接拒绝而不是让重复内容“确认自己”。
  if (relocalized && common.confidence < 0.35) {
    return {
      placement: null,
      nextTop: state.currentTop,
      match,
      relocalized: null,
      pendingJump: null,
      decision: makeDecision({
        ...common,
        accepted: false,
        reason: 'low-confidence',
        positionDelta: 0,
        appendRange: null,
      }),
    };
  }
  // 重复截图、嵌套浏览器画面等内容会产生“分数很好但位置完全错误”的单帧锚点。
  // 对低置信匹配、较大跳转以及 lost 后的无方向重定位统一要求独立第二帧确认。
  // 不能直接用 wheel 方向否决：tracking lost 时 currentTop 已经过期。
  const positionDelta = Math.abs(nextTop - state.currentTop);
  const riskyRelocalization = Boolean(relocalized) && (
    common.confidence < 0.55
    || positionDelta >= frame.height * 0.5
    || (expectedDirection === 0 && positionDelta >= frame.height * 0.25)
  );
  const riskyAdjacentMatch = !relocalized && accepted && common.confidence < 0.42;
  const requiresConfirmation = riskyRelocalization || riskyAdjacentMatch;
  const pendingTolerance = Math.max(8, frame.height * 0.08);
  const pendingConfirmed = accepted
    && state.pendingJump
    && Math.abs(state.pendingJump.top - nextTop) <= pendingTolerance;
  // 一旦上一帧进入确认态，本帧不得换一条路径接受相距很远的候选。
  // 否则 content 候选等待确认时，adjacent 匹配可以绕过门禁并直接写入错误位置。
  if (accepted && state.pendingJump && !pendingConfirmed) {
    return {
      placement: null,
      nextTop: state.currentTop,
      match,
      relocalized: null,
      pendingJump: null,
      decision: makeDecision({
        ...common,
        accepted: false,
        reason: 'ambiguous',
        positionDelta: 0,
        appendRange: null,
        confirmation: 'rejected',
      }),
    };
  }
  if (requiresConfirmation && !pendingConfirmed) {
    return {
      placement: null,
      nextTop: state.currentTop,
      match,
      relocalized: null,
      pendingJump: {
        source,
        top: Math.round(nextTop),
        confidence: common.confidence,
      },
      decision: makeDecision({
        ...common,
        accepted: false,
        reason: 'pending-confirmation',
        positionDelta: 0,
        appendRange: null,
        confirmation: 'required',
      }),
    };
  }
  if (!accepted) {
    return {
      placement: null,
      nextTop: state.currentTop,
      match,
      relocalized: null,
      pendingJump: null,
      decision: makeDecision({
        ...common,
        reason: rejectionReason(match, motionTimedOut),
      }),
    };
  }

  const placement = planPositionedIncrement(positionedFrameBounds(state.frames), nextTop, frame.height);
  const appendRange = placement.rowCount > 0
    ? { top: placement.targetTop, bottom: placement.targetTop + placement.rowCount }
    : null;
  return {
    placement,
    nextTop: Math.round(nextTop),
    match,
    relocalized,
    pendingJump: null,
    decision: makeDecision({
      ...common,
      accepted: true,
      reason: relocalized ? 'relocalized' : 'matched',
      appendRange,
      confirmation: pendingConfirmed ? 'confirmed' : 'none',
    }),
  };
}
