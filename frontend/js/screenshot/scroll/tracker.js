//! 长截图单帧追踪决策。纯算法层，不依赖 DOM / Tauri / 全局 session。

import {
  createGrayFingerprint, createVerticalReference,
  estimateVerticalShift, planPositionedIncrement, positionedFrameBounds,
  relocalizeFromKeyframes, relocalizeFromPositionedContent,
  validatePositionedOverlap,
} from './stitch.js';

export const SCROLL_DECISION_SCHEMA_VERSION = 2;
export const MAX_SCROLL_KEYFRAMES = 64;
export const TRACKING_LOST_FAILURE_THRESHOLD = 2;

/**
 * 生产采集与离线回放共用同一套 tracking → recovering → lost 状态转换。
 * 单帧拒绝先进入 recovering，避免动画或一次歧义立刻把预览染成橙色；连续失败
 * 才进入 lost。pending-confirmation / unchanged 不算失败，成功提交统一复位。
 */
export function transitionScrollTracking(state, tracked) {
  // 算法回放使用 trackingState/lostFrameCount；生产 ScrollCaptureSession 为兼容
  // ss 门面保留 scroll* 前缀。统一在纯函数入口归一化，避免两条状态流再次漂移。
  const currentTrackingState = state.trackingState ?? state.scrollTrackingState ?? 'tracking';
  const currentLostFrameCount = Math.max(
    0,
    state.lostFrameCount ?? state.scrollLostFrameCount ?? 0,
  );
  if (tracked.decision.accepted) {
    return { trackingState: 'tracking', lostFrameCount: 0, becameLost: false };
  }

  const rejectedRecovery = tracked.decision.reason === 'low-confidence'
    && tracked.decision.source !== 'adjacent';
  const hardFailure = tracked.match?.status === 'no-match'
    || rejectedRecovery
    || tracked.decision.confirmation === 'rejected';
  if (!hardFailure) {
    return {
      trackingState: currentTrackingState,
      lostFrameCount: currentLostFrameCount,
      becameLost: false,
    };
  }

  const lostFrameCount = currentLostFrameCount + 1;
  const trackingState = currentTrackingState === 'lost'
    || lostFrameCount >= TRACKING_LOST_FAILURE_THRESHOLD
    ? 'lost'
    : 'recovering';
  return {
    trackingState,
    lostFrameCount,
    becameLost: trackingState === 'lost' && currentTrackingState !== 'lost',
  };
}

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

function summarizeMatch(match) {
  if (!match) return null;
  return {
    status: match.status,
    reason: match.reason || null,
    shift: Number.isFinite(match.shift) ? match.shift : null,
    candidateShift: Number.isFinite(match.candidateShift) ? match.candidateShift : null,
    score: finiteOrNull(match.score),
    secondScore: finiteOrNull(match.secondScore),
    sameScore: finiteOrNull(match.sameScore),
    rivalShift: Number.isFinite(match.rivalShift) ? match.rivalShift : null,
    rivalScore: finiteOrNull(match.rivalScore),
  };
}

function positionedValidationFailed(validation) {
  return validation?.status === 'conflict' || validation?.status === 'insufficient-detail';
}

function positionedValidationNeedsRecovery(validation) {
  return positionedValidationFailed(validation) || validation?.status === 'insufficient';
}

function positionedValidationReason(validation) {
  if (validation?.status === 'conflict') return 'position-conflict';
  if (validation?.status === 'insufficient-detail') return 'insufficient-detail';
  if (validation?.status === 'insufficient') return 'insufficient-overlap';
  return 'no-overlap';
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
  let positionedOverlap = null;
  let relocalizedOverlap = null;
  let match = wasLost
    ? { status: 'no-match', shift: 0, score: Infinity, reason: 'tracking-lost' }
    : estimateVerticalShift(state.lastFrame, frame, {
      expectedDirection,
      strictDirection: expectedDirection !== 0,
      rejectAmbiguous: true,
  });
  const adjacentMatch = summarizeMatch(match);
  if (!wasLost && match.status === 'matched') {
    const adjacentTop = state.currentTop + match.shift;
    positionedOverlap = validatePositionedOverlap(state.frames, frame, adjacentTop);
    if (positionedValidationNeedsRecovery(positionedOverlap)) {
      match = {
        ...match,
        status: 'no-match',
        reason: positionedValidationReason(positionedOverlap),
        candidateShift: match.shift,
        shift: 0,
      };
    }
  }
  const validateRecoveryCandidate = (candidate) => {
    if (!candidate) return null;
    relocalizedOverlap = validatePositionedOverlap(state.frames, frame, candidate.top);
    // 恢复候选只要与已确认内容直接冲突就必须拒绝；连续两张相同截图不能替
    // 重复纹理“确认自己”。相邻位置证据不足时，恢复候选还必须给出绝对一致性。
    const recoveryConflicts = positionedValidationFailed(relocalizedOverlap);
    const recoveryLacksCorroboration = positionedValidationNeedsRecovery(positionedOverlap)
      && relocalizedOverlap.status !== 'consistent';
    if (recoveryConflicts || recoveryLacksCorroboration) {
      match = {
        ...candidate.match,
        status: 'no-match',
        reason: positionedValidationReason(
          recoveryConflicts ? relocalizedOverlap : positionedOverlap,
        ),
        candidateShift: candidate.top - state.currentTop,
        shift: 0,
      };
      return null;
    }
    // currentTop 在 lost 后只是最后一次已确认坐标，不能再承担物理距离门禁。
    // 候选已经过全局唯一性筛选和已拼像素复核；大跨度风险由下方的连续帧确认处理。
    return candidate;
  };
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
    relocalized = validateRecoveryCandidate(relocalized);
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
    relocalized = validateRecoveryCandidate(relocalized);
    if (relocalized) {
      match = relocalized.match;
      nextTop = relocalized.top;
    }
  }

  // recovering 时 lastFrame 仍是最后一张已确认画面；重新看到它本身就是最强的
  // “已滚回安全区域”证据。正常 tracking 下 unchanged 仍只表示没有发生滚动。
  const recoveredAdjacentUnchanged = state.trackingState === 'recovering'
    && !relocalized
    && match.status === 'unchanged';
  const accepted = (match.status === 'matched' || match.status === 'unchanged')
    && (relocalized || match.shift !== 0 || recoveredAdjacentUnchanged);
  const source = relocalized
    ? (relocalized.scope === 'content' ? 'content-partition' : `keyframe-${relocalized.scope}`)
    : (recoveredAdjacentUnchanged
      ? 'adjacent-recovery'
      : (attemptedRelocalization ? (wasLost ? 'keyframe-search' : 'adjacent+keyframe') : 'adjacent'));
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
    calibration: {
      adjacent: adjacentMatch,
      positionedOverlap,
      relocalizedOverlap,
      selected: summarizeMatch(match),
    },
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
      reason: relocalized || recoveredAdjacentUnchanged ? 'relocalized' : 'matched',
      appendRange,
      confirmation: pendingConfirmed ? 'confirmed' : 'none',
    }),
  };
}
