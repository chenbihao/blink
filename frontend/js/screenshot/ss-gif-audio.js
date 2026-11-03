import {invoke, normalizeError} from '../shared/tauri.js';

const $ = id => document.getElementById(id);
let generation = 0, deviceGeneration = 0, monitorTimer = null, recording = null, status = {}, busy = false;
let monitorQueue = Promise.resolve();
function setMonitor(options, captured) {
    const request = monitorQueue.catch(() => {}).then(() => {
        if (options && captured !== generation) return;
        return invoke('gif_audio_monitor', {options});
    });
    monitorQueue = request; return request;
}
const labels = ['麦克风', '系统声音'];
export function audioOptions() {
    return {mode: $('gif-audio-mode')?.value || 'silent', microphone: $('gif-audio-device-0')?.value || null, system: $('gif-audio-device-1')?.value || null};
}
export function enabledSources(mode) { return [mode === 'microphone' || mode === 'both', mode === 'system' || mode === 'both']; }
export function meterLevel(value) { return Math.max(0, Math.min(1, (20 * Math.log10(Math.max(0.001, Number(value) || 0)) + 60) / 60)); }
function message(text) { if ($('gif-audio-error')) $('gif-audio-error').textContent = text || ''; }
export function closeAudioSettings() {
    $('gif-audio-settings')?.classList.add('hidden');
    if (recording) return invoke('gif_overlay_focusable', {focusable: false}).catch(() => {});
    return Promise.resolve();
}
function renderMeters(container, value) {
    if (!container) return;
    for (let i = 0; i < 2; i++) {
        const track = value.tracks?.find(t => t.source === i), row = container.querySelector(`[data-source="${i}"]`);
        if (!row) continue;
        row.classList.toggle('hidden', !track);
        const state = value.error ? '设备异常' : !value.active ? '已暂停' : value.muted?.[i] ? '采集静音' : '采集中';
        row.title = `${labels[i]}：${track?.name || ''} · ${state} · 峰值 ${Math.round((value.peak?.[i] || 0) * 100)}%`;
        row.querySelector('.audio-source-name').textContent = `${labels[i]}：${track?.name || ''} · ${state}`;
        row.querySelector('.audio-level-fill').style.height = `${value.active && !value.error ? meterLevel(value.rms?.[i]) * 100 : 0}%`;
        row.querySelector('.audio-level-peak').style.bottom = `${value.active && !value.error ? meterLevel(value.peak?.[i]) * 100 : 0}%`;
        row.classList.toggle('audio-overload', value.active && (value.peak?.[i] || 0) >= 0.98);
        row.classList.toggle('audio-inactive', !value.active || !!value.error || !!value.muted?.[i]);
        row.setAttribute('aria-label', row.title);
    }
}
export function applyAudioStatus(value, sessionId) {
    if (sessionId !== recording) return;
    status = value || {}; renderMeters($('gif-rec-audio'), status);
    $('gif-audio-recover')?.classList.toggle('hidden', !status.error);
    if (status.error) message(status.error);
    $('gif-audio-mode').disabled = !!recording;
    for (let i = 0; i < 2; i++) $('gif-audio-device-' + i).disabled = !!recording && !status.error;
}
export async function refreshAudioDevices() {
    const captured = ++deviceGeneration;
    try {
        const devices = await invoke('gif_audio_devices');
        if (captured !== deviceGeneration) return;
        for (let i = 0; i < 2; i++) {
            const select = $('gif-audio-device-' + i), previous = select.value;
            select.replaceChildren();
            const defaultDevice = devices.find(d => d.source === i && d.isDefault);
            const option = document.createElement('option'); option.value = ''; option.textContent = `系统默认：${defaultDevice?.name || '无可用设备'}`; select.append(option);
            for (const d of devices.filter(d => d.source === i)) { const opt = document.createElement('option'); opt.value = d.id; opt.textContent = d.name; select.append(opt); }
            if (previous && !devices.some(d => d.source === i && d.id === previous)) { const opt = document.createElement('option'); opt.value = previous; opt.textContent = '所选设备已不可用（请重新选择）'; select.append(opt); }
            select.value = previous;
        }
    } catch (e) { if (captured === deviceGeneration) message(normalizeError(e).message); }
}
async function monitor() {
    if (recording) return;
    const captured = ++generation;
    message(''); clearInterval(monitorTimer);
    try {
        await setMonitor(audioOptions(), captured);
        if (captured !== generation || recording) return;
        monitorTimer = setInterval(async () => {
            if (busy) return; busy = true;
            try { const value = await invoke('gif_audio_monitor_status'); if (captured === generation && !recording) { renderMeters($('gif-prep-audio'), value); message(value.error); } }
            catch (e) { if (captured === generation) message(normalizeError(e).message); }
            finally { busy = false; }
        }, 150);
    } catch (e) { if (captured === generation) message(normalizeError(e).message); }
}
export function enterAudioPrep() {
    recording = null; status = {}; $('gif-audio-mode').disabled = false;
    for (let i = 0; i < 2; i++) $('gif-audio-device-' + i).disabled = false;
    void refreshAudioDevices(); void monitor();
}
export function stopAudioMonitor() {
    generation++; clearInterval(monitorTimer); monitorTimer = null;
    $('gif-audio-settings')?.classList.add('hidden');
    return setMonitor(null).catch(() => {});
}
export async function beginAudioRecording(sessionId) { await stopAudioMonitor(); recording = sessionId; }
export function endAudioRecording() { recording = null; status = {}; void stopAudioMonitor(); }
export function bindAudioControls() {
    for (const id of ['gif-audio-open', 'gif-audio-recover']) $(id)?.addEventListener('click', () => {
        $('gif-audio-settings').classList.toggle('hidden'); void refreshAudioDevices();
        if (recording) void invoke('gif_overlay_focusable', {focusable: !$('gif-audio-settings').classList.contains('hidden')}).catch(() => {});
    });
    $('gif-audio-close')?.addEventListener('click', closeAudioSettings);
    for (const id of ['gif-audio-mode', 'gif-audio-device-0', 'gif-audio-device-1']) $(id)?.addEventListener('change', async () => {
        const enabled = enabledSources(audioOptions().mode);
        for (let i = 0; i < 2; i++) $('gif-audio-field-' + i)?.classList.toggle('hidden', !enabled[i]);
        if (recording) {
            try { await invoke('gif_audio_control', {sessionId: recording, options: audioOptions()}); message('设备已选择，请点击继续录制以核验并恢复'); }
            catch (e) { message(normalizeError(e).message); }
        } else { void monitor(); }
    });
    $('gif-rec-audio')?.addEventListener('click', async e => {
        const row = e.target.closest('[data-source]'); if (!row || !recording) return;
        const source = Number(row.dataset.source), muted = !status.muted?.[source];
        try { await invoke('gif_audio_control', {sessionId: recording, source, muted}); status.muted ||= [false, false]; status.muted[source] = muted; renderMeters($('gif-rec-audio'), status); }
        catch (e) { message(normalizeError(e).message); }
    });
}
