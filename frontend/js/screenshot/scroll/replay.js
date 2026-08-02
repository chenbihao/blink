//! 长截图离线回放执行器。输入已解码的完整帧，复用生产追踪决策并返回确定性摘要。

import { commitTrackedFrame } from './stitch.js';
import { rememberScrollKeyframe, trackScrollFrame } from './tracker.js';

export function replayScrollSequence(sequence) {
  const state = {
    frames: [],
    keyframes: [],
    lastFrame: null,
    currentTop: 0,
    trackingState: 'tracking',
    pendingJump: null,
    lostFrameCount: 0,
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
      const rejectedRecovery = tracked.decision.reason === 'low-confidence'
        && tracked.decision.source !== 'adjacent';
      if (tracked.match?.status === 'no-match' || rejectedRecovery) {
        state.trackingState = 'lost';
      }
      if (tracked.match?.status === 'no-match' || rejectedRecovery
          || tracked.decision.reason === 'pending-confirmation') {
        state.lostFrameCount++;
      }
      continue;
    }
    const committed = commitTrackedFrame(state.frames, state.lastFrame, captured.frame, tracked);
    state.frames = committed.frames;
    state.currentTop = tracked.nextTop;
    state.lastFrame = captured.frame;
    state.trackingState = 'tracking';
    state.lostFrameCount = 0;
    state.keyframes = rememberScrollKeyframe(
      state.keyframes,
      committed.committedFrame,
      state.currentTop,
    );
  }
  return {
    decisions,
    confirmedTop: state.currentTop,
    confirmedFrames: state.frames,
  };
}
