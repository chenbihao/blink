//! 长截图单帧追踪决策。纯算法层，不依赖 DOM / Tauri / 全局 session。

import {
  createGrayFingerprint, createVerticalReference,
  estimateVerticalShift, planPositionedIncrement, positionedFrameBounds,
  relocalizeFromKeyframes, relocalizeFromPositionedContent,
} from './ss-scroll-stitch.js';

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
  const largeContentJump = relocalized?.scope === 'content'
    && Math.abs(nextTop - state.currentTop) >= frame.height * 0.75;
  const pendingTolerance = Math.max(8, frame.height * 0.35);
  const pendingConfirmed = largeContentJump
    && state.pendingJump?.source === 'content-partition'
    && Math.abs(state.pendingJump.top - nextTop) <= pendingTolerance;
  if (largeContentJump && !pendingConfirmed) {
    return {
      placement: null,
      nextTop: state.currentTop,
      match,
      relocalized: null,
      pendingJump: {
        source: 'content-partition',
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
