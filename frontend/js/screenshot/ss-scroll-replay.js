//! 长截图离线回放执行器。输入已解码的完整帧，复用生产追踪决策并返回确定性摘要。

import { extractRows } from './ss-scroll-stitch.js';
import { rememberScrollKeyframe, trackScrollFrame } from './ss-scroll-tracker.js';

export function replayScrollSequence(sequence) {
  const state = {
    frames: [],
    keyframes: [],
    lastFrame: null,
    currentTop: 0,
    trackingState: 'tracking',
    pendingJump: null,
  };
  const decisions = [];
  for (const captured of sequence) {
    const tracked = trackScrollFrame(state, captured.frame, {
      expectedDirection: captured.expectedDirection,
      motionTimedOut: captured.settle?.timedOut === true,
    });
    decisions.push(tracked.decision);
    state.pendingJump = tracked.pendingJump;
    if (!tracked.decision.accepted) {
      if (tracked.match?.status === 'no-match') state.trackingState = 'lost';
      continue;
    }
    const placement = tracked.placement;
    if (placement.rowCount > 0) {
      const increment = extractRows(captured.frame, placement.startRow, placement.rowCount);
      if (increment) state.frames.push({ image: increment, top: placement.targetTop });
    }
    state.currentTop = tracked.nextTop;
    state.lastFrame = captured.frame;
    state.trackingState = 'tracking';
    state.keyframes = rememberScrollKeyframe(state.keyframes, captured.frame, state.currentTop);
  }
  return {
    decisions,
    confirmedTop: state.currentTop,
    confirmedFrames: state.frames,
  };
}
