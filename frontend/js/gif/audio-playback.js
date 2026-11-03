/** 编辑后时间 → 原始源轨时间。停留新增部分保持静音，悬浮/拖动不触发播放。 */
export function locateAudio(spans, outputMs) {
    let at = 0;
    for (let i = 0; i < (spans || []).length; i++) {
        const s = spans[i], end = at + s.outputMs;
        if (outputMs < end) {
            const rate = (s.soundMs + s.silenceMs) / s.outputMs;
            const offset = Math.max(0, outputMs - at) * rate;
            return {span: i, sourceMs: s.sourceMs + Math.min(offset, s.soundMs), rate, silent: offset >= s.soundMs};
        }
        at = end;
    }
    return null;
}

/** 画面/音轨由编辑器同一个绝对单调时钟驱动；每次进度刷新校正设备播放时钟。 */
export function createAudioPlayback({onError = () => {}} = {}) {
    let epoch = 0, tracks = [], context = null, ready = Promise.resolve();
    function pause() { for (const t of tracks) { t.element.pause(); t.playing = false; } if (context?.state === 'running') void context.suspend().catch(() => {}); }
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
            if (!audible) { if (t.playing) t.element.pause(); t.playing = false; continue; }
            // MediaElement preservesPitch 在变速时保持音调；WebAudio gain 支持 0–200%。
            if (!context) context = new AudioContext();
            if (!t.source) { t.source = context.createMediaElementSource(t.element); t.gain = context.createGain(); t.source.connect(t.gain).connect(context.destination); }
            if (context.state === 'suspended') void context.resume().catch(onError);
            t.gain.gain.value = setting.gain;
            const target = position.sourceMs / 1000;
            if (t.span !== position.span || Math.abs(t.element.currentTime - target) > 0.04) t.element.currentTime = target;
            t.span = position.span; t.element.playbackRate = position.rate;
            if (!t.playing) { t.playing = true; const captured = epoch; t.element.play().catch(error => { if (captured === epoch && t.playing) { t.playing = false; onError(error); } }); }
        }
    }
    return {load, sync, pause, clear, ready: () => ready, dispose() { clear(); void context?.close(); }};
}
