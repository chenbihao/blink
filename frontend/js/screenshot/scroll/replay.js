//! 离线回放与 Worker 共用同一状态机，逐帧决策保持可对拍。
import {ScrollTrackingEngine} from './tracking-engine.js';

export function replayScrollSequence(sequence, limits) {
    let engine = new ScrollTrackingEngine({}, limits);
    const decisions = sequence.map(captured => {
        if (captured.trackingReset) engine = new ScrollTrackingEngine({
            frames: engine.state.frames, currentTop: engine.state.currentTop,
            height: captured.frame.height, trackingState: engine.state.trackingState,
        }, limits);
        return engine.process(captured.frame, {
        expectedDirection: captured.expectedDirection,
        motionTimedOut: captured.settle?.timedOut === true,
        discontinuity: captured.discontinuity === true,
        }).decision;
    });
    return {decisions, confirmedTop: engine.state.currentTop, confirmedFrames: engine.state.frames};
}
