import {t, onLangChange} from '../i18n/index.js';
import {invoke, normalizeError} from '../shared/tauri.js';
import {ss} from './ss-state.js';
import {recordingSession as state} from './recording-session.js';
import {openRecordingPanel, closeRecordingPanel, recordingUiChanged, syncRecordingSelect, showRecordingMessage, nextRecordingValue, bindRecordingWheel} from './ss-recording-ui.js';

const $ = id => document.getElementById(id);
let generation = 0, deviceGeneration = 0, monitorTimer = null, status = {}, busy = false;
let monitorQueue = Promise.resolve();
const METER_INTERVAL_MS = 150;
function setMonitor(options, captured) {
    const request = monitorQueue.catch(() => {}).then(() => {
        if (options && captured !== generation) return;
        return invoke('gif_audio_monitor', {options});
    });
    monitorQueue = request; return request;
}
const labels = () => [t('gif.audio.microphone'), t('gif.audio.system')];
export function audioOptions() {
    return {mode: $('gif-audio-mode')?.value || 'silent', microphone: $('gif-audio-device-0')?.value || null, system: $('gif-audio-device-1')?.value || null};
}
export function enabledSources(mode) { return [mode === 'microphone' || mode === 'both', mode === 'system' || mode === 'both']; }
export function meterLevel(value) { return Math.max(0, Math.min(1, (20 * Math.log10(Math.max(0.001, Number(value) || 0)) + 60) / 60)); }
function message(text) { if ($('gif-audio-error')) $('gif-audio-error').textContent = text || ''; showRecordingMessage(text); }
export function closeAudioSettings() {
    closeRecordingPanel();
    $('gif-audio-settings')?.classList.add('hidden');
    return Promise.resolve();
}
const peakHolds = new WeakMap();
function renderMeters(container, value) {
    if (!container) return;
    for (let i = 0; i < 2; i++) {
        const track = value.tracks?.find(t => t.source === i), row = container.querySelector(`[data-source="${i}"]`);
        if (!row) continue;
        const enabled = enabledSources(audioOptions().mode)[i];
        row.classList.toggle('audio-disabled', !enabled);
        const sourceError = value.sourceErrors ? value.sourceErrors[i] : value.error;
        const active = !!track && value.active && !sourceError && !value.muted?.[i];
        const trackState = t('gif.rec.' + (sourceError ? 'deviceError' : value.muted?.[i] ? 'muted' :
            !active ? state.preparing ? 'noSignal' : 'paused' : state.preparing ? 'testing' : 'capturing'));
        const peak = active ? Math.max(0, Number(value.peak?.[i]) || 0) : 0;
        const now = performance.now(), previous = peakHolds.get(row);
        const hold = !active ? {peak: 0, until: 0} : !previous || peak >= previous.peak || now >= previous.until
            ? {peak, until: now + 1000} : previous;
        peakHolds.set(row, hold);
        row.style.setProperty('--audio-level', active ? meterLevel(value.rms?.[i]) : 0);
        row.style.setProperty('--audio-peak', meterLevel(hold.peak));
        row.title = `${labels()[i]}：${track?.name || ''} · ${trackState} · ${peak > 0 ? (20 * Math.log10(peak)).toFixed(1) + ' dBFS' : t('gif.rec.noSignal')}${hold.peak >= 0.98 ? ' · ' + t('gif.rec.nearClip') : ''}`;
        row.classList.toggle('audio-overload', active && hold.peak >= 0.98);
        row.classList.toggle('audio-inactive', !active);
        row.setAttribute('aria-label', row.title);
        const mute = document.querySelector(`[data-audio-mute="${i}"]`);
        if (mute) {
            mute.hidden = !state.active;
            mute.disabled = !state.active || state.busy || !enabled;
            mute.setAttribute('aria-pressed', String(!!value.muted?.[i]));
            mute.textContent = t('gif.rec.' + (value.muted?.[i] ? 'unmute' : 'mute'));
        }
    }
}
function renderAudioMeters(value) {
    renderMeters($('gif-audio-meters'), value);
    renderMeters($('gif-audio-settings'), value);
    $('gif-audio-open')?.classList.toggle('warn', !!value.error);
}
export function applyAudioStatus(value, sessionId) {
    if (sessionId !== state.sessionId) return;
    status = value || {}; renderAudioMeters(status);
    $('gif-audio-open')?.classList.toggle('warn', !!status.error);
    if (status.error) message(status.error);
    $('gif-audio-mode').disabled = !state.preparing;
    syncRecordingSelect('gif-audio-mode');
    for (let i = 0; i < 2; i++) {
        $('gif-audio-device-' + i).disabled = !state.preparing && (state.mode !== 'paused' || !!status.active);
        for (const button of $('gif-audio-devices-' + i)?.children || []) button.disabled = !state.preparing && (state.mode !== 'paused' || !!status.active);
    }
    updateSourceFields();
}
export async function refreshAudioDevices() {
    const captured = ++deviceGeneration;
    try {
        const devices = await invoke('gif_audio_devices');
        if (captured !== deviceGeneration) return;
        for (let i = 0; i < 2; i++) {
            const select = $('gif-audio-device-' + i), previous = select.value;
            const list = $('gif-audio-devices-' + i); list.replaceChildren();
            const defaultDevice = devices.find(d => d.source === i && d.isDefault);
            const options = [{id: '', name: t('gif.rec.defaultDevice', {name:defaultDevice?.name || t('gif.rec.noDevice')})}, ...devices.filter(d => d.source === i)];
            if (previous && !options.some(d => d.id === previous)) options.push({id: previous, name: t('gif.rec.deviceMissing')});
            for (const device of options) {
                const button = document.createElement('button');
                button.type = 'button'; button.className = 'recording-option'; button.textContent = device.name;
                button.setAttribute('aria-pressed', String(device.id === previous));
                button.disabled = !state.preparing && (state.mode !== 'paused' || !!status.active);
                button.addEventListener('click', () => {
                    select.value = device.id;
                    for (const sibling of list.children) sibling.setAttribute('aria-pressed', String(sibling === button));
                    select.dispatchEvent(new Event('change', {bubbles: true}));
                });
                list.append(button);
            }
            select.value = previous;
        }
    } catch (e) { if (captured === deviceGeneration) message(normalizeError(e).message); }
}
async function pollMonitor() {
    if (busy || !state.preparing) return;
    const captured = generation;
    busy = true;
    try {
        const value = await invoke('gif_audio_monitor_status');
        if (captured !== generation || !state.preparing) return;
        const changed = value.error !== status.error;
        status = value; renderAudioMeters(value);
        if (changed) message(value.error);
    } catch (e) { if (captured === generation) message(normalizeError(e).message); }
    finally { busy = false; }
}
async function monitor() {
    if (!state.preparing) return;
    const captured = ++generation;
    message('');
    // 设备配置在后台进行，电平轮询持续读取另一条仍在工作的流。
    if (!monitorTimer) monitorTimer = setInterval(pollMonitor, METER_INTERVAL_MS);
    try { await setMonitor(audioOptions(), captured); }
    catch (e) { if (captured === generation) message(normalizeError(e).message); }
}
export function enterAudioPrep() {
    status = {}; $('gif-audio-mode').disabled = !state.preparing;
    syncRecordingSelect('gif-audio-mode');
    updateSourceFields();
    renderAudioMeters({});
    for (let i = 0; i < 2; i++) $('gif-audio-device-' + i).disabled = false;
    void refreshAudioDevices(); void monitor();
}
export function stopAudioMonitor() {
    generation++; deviceGeneration++; clearInterval(monitorTimer); monitorTimer = null;
    closeRecordingPanel();
    $('gif-audio-settings')?.classList.add('hidden');
    return setMonitor(null).catch(() => {});
}
export async function beginAudioRecording(sessionId, options) {
    await stopAudioMonitor(); if(state.sessionId !== sessionId) return;
    if (options) {
        $('gif-audio-mode').value = options.mode;
        $('gif-audio-device-0').value = options.microphone || '';
        $('gif-audio-device-1').value = options.system || '';
    }
    $('gif-audio-mode').disabled = true;
    renderAudioMeters({}); updateSourceFields();
}
export function endAudioRecording() { status = {}; void stopAudioMonitor(); }
export function bindAudioControls() {
    const cycle = (step = 1, wrap = true) => {
        if (!state.preparing || state.busy) return;
        closeRecordingPanel();
        const next = nextRecordingValue(['silent', 'microphone', 'system', 'both'], audioOptions().mode, step, wrap);
        if (next === audioOptions().mode) return;
        $('gif-audio-mode').value = next;
        $('gif-audio-mode').dispatchEvent(new Event('change', {bubbles: true}));
    };
    $('gif-audio-open')?.addEventListener('click', () => cycle());
    bindRecordingWheel($('gif-audio-open')?.closest('.recording-split'), step => cycle(step, false), () => state.preparing && !state.busy);
    $('gif-audio-menu')?.addEventListener('click', () => {
        if (state.busy) return;
        const panel = $('gif-audio-settings'), anchor = $('gif-audio-menu');
        const opened = openRecordingPanel(panel, anchor);
        if (opened) {
            updateSourceFields(); void refreshAudioDevices();
        }
    });
    $('gif-audio-close')?.addEventListener('click', closeAudioSettings);
    for (const button of document.querySelectorAll('[data-audio-mode]')) button.addEventListener('click', () => {
        if (!state.preparing) return;
        $('gif-audio-mode').value = button.dataset.audioMode;
        $('gif-audio-mode').dispatchEvent(new Event('change', {bubbles: true}));
    });
    for (const id of ['gif-audio-mode', 'gif-audio-device-0', 'gif-audio-device-1']) $(id)?.addEventListener('change', async () => {
        updateSourceFields();
        if (state.sessionId) {
            const sessionId = state.sessionId;
            try { await invoke('gif_audio_control', {sessionId, options: audioOptions()}); if (state.sessionId === sessionId) message(t('gif.rec.selectedDevice')); }
            catch (e) { if (state.sessionId === sessionId) message(normalizeError(e).message); }
        } else { void monitor(); }
    });
    for (const button of document.querySelectorAll('[data-audio-mute]')) button.addEventListener('click', async () => {
        if (!state.sessionId) return;
        const sessionId = state.sessionId, source = Number(button.dataset.audioMute), muted = !status.muted?.[source];
        try { await invoke('gif_audio_control', {sessionId, source, muted}); if (sessionId !== state.sessionId) return;
            status.muted ||= [false, false]; status.muted[source] = muted; renderAudioMeters(status); }
        catch (e) { if (state.sessionId === sessionId) message(normalizeError(e).message); }
    });
    ss._syncRecordingAudio=updateSourceFields;
    onLangChange(() => {updateSourceFields(); renderAudioMeters(status);});
}

function updateSourceFields() {
    const enabled = enabledSources(audioOptions().mode);
    for (let i = 0; i < 2; i++) $('gif-audio-field-' + i)?.classList.toggle('hidden', !enabled[i]);
    const canChangeDevice=state.preparing || (state.mode==='paused' && !status.active);
    for(let i=0;i<2;i++) {
        if($('gif-audio-device-'+i)) $('gif-audio-device-'+i).disabled=!canChangeDevice;
        for(const button of $('gif-audio-devices-'+i)?.children || []) button.disabled=!canChangeDevice;
    }
    const mode = audioOptions().mode;
    const keys = {silent:'gif.rec.source.silent',microphone:'gif.rec.source.microphone',system:'gif.rec.source.system',both:'gif.rec.source.both'};
    if ($('gif-audio-label')) $('gif-audio-label').textContent = t(keys[mode] || keys.silent);
    if ($('gif-audio-open')) {
        $('gif-audio-open').closest('.recording-split')?.setAttribute('aria-label', t('gif.rec.sound'));
        $('gif-audio-open').disabled = !state.preparing || state.busy;
        $('gif-audio-open').title = `${t('gif.rec.sound')}：${t(keys[mode] || keys.silent)}${state.preparing ? ' · ' + t('gif.rec.clickCycle') : ''}`;
        $('gif-audio-open').setAttribute('aria-label', $('gif-audio-open').title);
    }
    if ($('gif-audio-menu')) {
        $('gif-audio-menu').disabled = state.busy;
        $('gif-audio-menu').title = t('gif.rec.soundOptions');
        $('gif-audio-menu').setAttribute('aria-label', $('gif-audio-menu').title);
    }
    for (const button of document.querySelectorAll('[data-audio-mode]')) {
        button.disabled = !state.preparing;
        button.setAttribute('aria-pressed', String(button.dataset.audioMode === mode));
    }
    if ($('gif-audio-device-note')) $('gif-audio-device-note').textContent = t('gif.rec.' + (state.preparing ? 'previewNote' : state.mode === 'paused' ? 'pausedDeviceNote' : 'recordingDeviceNote'));
    recordingUiChanged();
}
