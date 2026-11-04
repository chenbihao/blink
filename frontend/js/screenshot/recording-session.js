//! 录制交互唯一状态：模式决定工具栏、可用操作和异步请求的生命周期。
const MODES = new Set(['screenshot', 'ready', 'starting', 'recording', 'paused', 'stopping']);

export function createRecordingSession() {
    return {
        mode: 'screenshot', epoch: 0, sessionId: null, drawing: false,
        snapshotRevision: 0, elapsedBase: 0, elapsedAt: 0, frames: 0,
        regionPhys: null, storage: null, targetFps: 15, actualFps: null,
        audioError: null, restoredControlsOnly: false,
        tickTimer: 0, hitRectTimer: 0, cancelArmed: false,
        textInputs: new Set(), resumeAfterText: false,
        get active() { return this.sessionId !== null; },
        get stopping() { return this.mode === 'stopping'; },
        get desktopMode() { return !this.drawing; },
        get preparing() { return this.mode === 'ready'; },
        get busy() { return this.mode === 'starting' || this.mode === 'stopping'; },
        get visible() { return this.mode !== 'screenshot'; },
        setMode(mode) {
            if (!MODES.has(mode)) throw new TypeError(`Invalid recording mode: ${mode}`);
            this.mode = mode;
        },
        reset(mode = 'screenshot') {
            if (!MODES.has(mode)) throw new TypeError(`Invalid recording mode: ${mode}`);
            this.epoch++;
            Object.assign(this, {
                mode, sessionId:null, drawing:false, snapshotRevision:0,
                elapsedBase:0, elapsedAt:0, frames:0, regionPhys:null,
                storage:null, actualFps:null, audioError:null, restoredControlsOnly:false,
                tickTimer:0, hitRectTimer:0, cancelArmed:false, resumeAfterText:false,
            });
            this.textInputs.clear();
        },
        owns(epoch, sessionId = this.sessionId) {
            return this.epoch === epoch && this.sessionId === sessionId && this.visible;
        },
    };
}

export const recordingSession = createRecordingSession();
