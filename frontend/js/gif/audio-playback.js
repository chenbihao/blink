/** 编辑后时间 → 原始源轨时间。停留新增部分保持静音，悬浮/拖动不触发播放。 */
export function locateAudio(spans, outputMs) {
    let at = 0;
    for (let i = 0; i < (spans || []).length; i++) {
        const s = spans[i], end = at + s.outputMs;
        if (outputMs < end) {
            const rate = s.speed ?? (s.soundMs + s.silenceMs) / s.outputMs;
            const offset = Math.max(0, outputMs - at) * rate;
            return {span: s.key ?? i, sourceMs: s.sourceMs + Math.min(offset, s.soundMs), rate, silent: offset >= s.soundMs};
        }
        at = end;
    }
    return null;
}

// 与后端 gif/audio.rs 相同的软限幅；0–90% 线性，峰值平滑收敛到 98%。
export function softLimit(value) {
    if (!Number.isFinite(value)) return 0;
    const magnitude = Math.abs(value);
    return magnitude <= 0.9 ? value : Math.sign(value) * (0.9 + 0.08 * (1 - Math.exp(-(magnitude - 0.9) / 0.08)));
}
let limiterCurve = null;
export function mixLimiterCurve() {
    // WaveShaper 输入域固定为 [-1,1]；先将最大 2×200% 混音缩为 1/4，
    // 曲线恢复线性区增益再限幅，避免输入先被 WaveShaper 的边界硬截断。
    return limiterCurve ||= Float32Array.from({length: 65537}, (_, i) => softLimit((i / 65536 * 2 - 1) * 4));
}
/** 画面/音轨由编辑器同一个绝对单调时钟驱动；每次进度刷新校正设备播放时钟。 */
export function createAudioPlayback({onError = () => {}} = {}) {
    let epoch = 0, tracks = [], context = null, bus = null, ready = Promise.resolve();
    let wantRunning = false, contextOperation = null;
    function driveContext() {
        if (!context || contextOperation || context.state === 'closed' || context.state === (wantRunning ? 'running' : 'suspended')) return;
        const current = context, target = wantRunning;
        const task = Promise.resolve(target ? current.resume() : current.suspend());
        contextOperation = task;
        void task.catch(error => { if (current === context && wantRunning) onError(error); }).finally(() => {
            if (contextOperation !== task) return;
            contextOperation = null;
            // suspend 的返回可能晚于下一次开启；完成后必须兑现最新意图。
            if (current === context && target !== wantRunning) driveContext();
        });
    }
    function stopTrack(t) {
        t.playSerial = (t.playSerial || 0) + 1;
        t.element.pause(); t.playing = false; t.span = -1;
    }
    function pause() {
        for (const t of tracks) stopTrack(t);
        wantRunning = false; driveContext();
    }
    function clear() {
        epoch++; pause();
        for (const t of tracks) { t.cancelLoad?.(); t.element.removeAttribute('src'); t.element.load(); t.source?.disconnect(); t.gain?.disconnect(); }
        tracks = [];
    }
    async function load(sessionId, enabled) {
        clear(); const captured = epoch;
        ready = Promise.all((enabled || []).map(async (active, sourceIndex) => {
            if (!active) return;
            if (captured !== epoch) return;
            const url = `http://blink-recording-audio.localhost/${encodeURIComponent(sessionId)}/${sourceIndex}`;
            const element = new Audio(); element.crossOrigin = 'anonymous'; element.src = url; element.preload = 'metadata'; element.preservesPitch = true;
            const track = {element, url, sourceIndex, span: -1, playing: false}; tracks.push(track);
            await new Promise((resolve, reject) => {
                track.cancelLoad = resolve;
                element.addEventListener('loadedmetadata', resolve, {once: true});
                element.addEventListener('error', () => reject(new Error('声音素材无法播放')), {once: true});
                element.load();
            });
        })).catch(error => { if (captured === epoch) { clear(); onError(error); } });
        return ready;
    }
    function sync(outputMs, playing, spans, mix) {
        const position = locateAudio(spans, outputMs);
        for (const t of tracks) {
            const setting = mix?.[t.sourceIndex] || {gain: 1, muted: false};
            const audible = playing && position && !position.silent && !setting.muted && setting.gain > 0;
            if (!audible) { if (t.playing) stopTrack(t); continue; }
            // MediaElement preservesPitch 在变速时保持音调；WebAudio gain 支持 0–200%。
            if (!context) {
                context = new AudioContext();
                bus = context.createGain(); bus.gain.value = 0.25;
                const limiter = context.createWaveShaper(); limiter.curve = mixLimiterCurve(); limiter.oversample = '4x';
                bus.connect(limiter).connect(context.destination);
            }
            if (!t.source) { t.source = context.createMediaElementSource(t.element); t.gain = context.createGain(); t.source.connect(t.gain).connect(bus); }
            wantRunning = true; driveContext();
            // 仅在用户调整时平滑音量；避免每次时钟刷新重启参数曲线。
            if (t.targetGain !== setting.gain) {
                if (t.targetGain == null) t.gain.gain.value = setting.gain;
                else t.gain.gain.setTargetAtTime(setting.gain, context.currentTime, 0.005);
                t.targetGain = setting.gain;
            }
            const target = position.sourceMs / 1000;
            if (t.span !== position.span || Math.abs(t.element.currentTime - target) > 0.04) t.element.currentTime = target;
            t.span = position.span; t.element.playbackRate = position.rate;
            if (!t.playing || t.element.paused) {
                t.playing = true;
                const captured = epoch, serial = t.playSerial = (t.playSerial || 0) + 1;
                t.element.play().catch(error => {
                    if (captured !== epoch || serial !== t.playSerial) return;
                    t.playing = false;
                    if (error.name !== 'AbortError') onError(error);
                });
            }
        }
    }
    return {load, sync, pause, clear, ready: () => ready, dispose() {
        clear(); const previous = context; context = null; bus = null; contextOperation = null;
        void previous?.close().catch(() => {});
    }};
}
