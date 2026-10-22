//! Worker 与离线回放共用的唯一追踪/提交状态机。无 DOM、IPC 或平台依赖。
import {compositePositionedFrames, extractPositionedViewport, prepareScrollCommit} from './stitch.js';
import {rememberScrollKeyframe, trackScrollFrame, transitionScrollTracking} from './tracker.js';

export class ScrollTrackingEngine {
    constructor(seed = {}, limits) {
        this.limits = limits;
        this.state = {
            frames: seed.frames || [], keyframes: [], currentTop: seed.currentTop || 0,
            lastFrame: null, trackingState: seed.trackingState || 'tracking',
            lostFrameCount: 0, pendingJump: null,
        };
        if (this.state.frames.length) {
            this.state.lastFrame = extractPositionedViewport(this.state.frames, this.state.currentTop, seed.height);
            if (!this.state.lastFrame) throw new Error('已确认长图无法恢复当前视口');
            this.state.keyframes = rememberScrollKeyframe([], this.state.lastFrame, this.state.currentTop);
        }
        this.imageIds = new WeakMap();
        this.nextImageId = 0;
        this.published = new Set();
    }

    process(frame, options = {}) {
        const state = this.state;
        // 缓冲耗尽后无法保证观测连续性，必须从已确认像素恢复，不能猜测新增内容。
        if (options.discontinuity && state.lastFrame) {
            state.trackingState = 'lost';
            state.pendingJump = null;
        }
        const before = state.trackingState;
        const startedAt = performance.now();
        const tracked = trackScrollFrame(state, frame, options);
        const trackedAt = performance.now();
        const committed = tracked.decision.accepted
            ? prepareScrollCommit(state.frames, state.lastFrame, frame, tracked, this.limits) : null;
        const limited = tracked.decision.accepted && !committed;
        const decision = limited
            ? {...tracked.decision, accepted: false, reason: 'frame-limit', appendRange: null}
            : tracked.decision;
        const tracking = limited
            ? {trackingState: state.trackingState, lostFrameCount: state.lostFrameCount, becameLost: false}
            : transitionScrollTracking(state, tracked);
        const previousTop = state.currentTop;
        if (!limited) {
            state.pendingJump = tracked.pendingJump;
            state.trackingState = tracking.trackingState;
            state.lostFrameCount = tracking.lostFrameCount;
            if (committed) {
                state.frames = committed.frames;
                state.currentTop = tracked.nextTop;
                state.lastFrame = frame;
                state.keyframes = rememberScrollKeyframe(state.keyframes, committed.committedFrame, state.currentTop);
            }
        }
        return {
            decision, match: tracked.match, reason: decision.reason,
            moved: decision.accepted, appended: (committed?.addedRows || 0) > 0,
            addedRows: committed?.addedRows || 0, positionShift: state.currentTop - previousTop,
            relocalized: tracked.relocalized?.scope, becameLost: tracking.becameLost,
            currentTop: state.currentTop, trackingState: state.trackingState,
            lostFrameCount: state.lostFrameCount, pendingJump: state.pendingJump,
            tracking: {before, after: state.trackingState, failureCount: state.lostFrameCount,
                becameLost: tracking.becameLost},
            timing: {trackMs: trackedAt - startedAt, commitMs: performance.now() - trackedAt},
        };
    }

    /** 只复制新增/替换片段。历史片段按 id 复用，避免逐帧传整张长图。 */
    takeSegments() {
        const nextPublished = new Set();
        const segments = this.state.frames.map(({image, top}) => {
            if (!this.imageIds.has(image)) this.imageIds.set(image, ++this.nextImageId);
            const id = this.imageIds.get(image);
            nextPublished.add(id);
            return {id, top, width: image.width, height: image.height,
                buffer: this.published.has(id) ? null : image.data.slice().buffer};
        });
        this.published = nextPublished;
        return segments;
    }

    compose() { return compositePositionedFrames(this.state.frames)?.image || null; }
}
