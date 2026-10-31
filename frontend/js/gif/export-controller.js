import {invoke, listen, normalizeError, saveDialog} from '../shared/tauri.js';
import {ExportJobState, exportPath, isTerminal, progressView} from './export-state.js';

/** 后端任务是真源；监听器丢失/弹窗关闭后由状态查询恢复，安装后由用户再次导出。 */
export function createExportController({documentRef = document, windowRef = window} = {}) {
    const $ = id => documentRef.getElementById(id);
    const els = Object.fromEntries(['export-dialog','export-format','export-path','export-size','export-quality',
        'export-note','export-engine','export-engine-text','btn-export-install','btn-export','btn-pick-path',
        'btn-export-confirm','btn-export-cancel-req','btn-export-close','btn-export-folder','export-progress-row',
        'export-progress','export-progress-text'].map(id => [id, $(id)]));
    const state = new ExportJobState();
    let epoch = 0, queryVersion = 0, timer = null, starting = false, disposed = false;
    let availability = null, availabilityError = '', availabilityVersion = 0;
    let statusBusy = false, statusAgain = null, unlisten = null;
    const format = () => els['export-format'].value;
    const busy = () => starting || state.running;
    const hint = text => { els['export-progress-text'].textContent = text; els['export-progress-row'].classList.remove('hidden'); };
    function render() {
        const mp4 = format() === 'mp4';
        els['export-note'].textContent = mp4 ? 'MP4 · H.264 · 30fps · 无声 · 单次播放；循环设置仅用于 GIF。' : 'GIF 动图 · 使用时间轴的循环设置。';
        els['export-engine'].classList.toggle('hidden', !mp4 || availability?.state === 'ready');
        els['export-engine-text'].textContent = availabilityError || (!availability ? '正在检查媒体引擎…' : 'MP4 需要媒体引擎，请在设置中安装或修复 FFmpeg，然后返回此处导出。');
        els['btn-export-confirm'].disabled = !state.sessionId || busy() || (mp4 && availability?.state !== 'ready');
        els['btn-export-cancel-req'].disabled = !state.running;
        for (const id of ['export-format','export-size','export-quality','btn-pick-path']) els[id].disabled = busy();
        els['btn-export-close'].textContent = busy() ? '后台继续' : '关闭';
        const labels = mp4 ? ['高（较大文件）','中（推荐）','低（较小文件）'] : ['高','中','低（最快）'];
        Array.from(els['export-quality'].options).forEach((option, index) => { option.textContent = labels[index]; });
        const snapshot = state.snapshot;
        if (!snapshot) return;
        const view = progressView(snapshot);
        els['export-progress-row'].classList.remove('hidden');
        els['export-progress'].value = view.value;
        hint(view.text);
        if (snapshot.stage === 'done' && snapshot.stats) {
            const s = snapshot.stats;
            const duration = snapshot.format === 'mp4' ? ` · ${(s.durationMs / 1000).toFixed(2)}s · 无声` : '';
            hint(`完成：${snapshot.format.toUpperCase()} · ${s.width}×${s.height} · ${s.writtenFrames} 帧${duration} · ${(s.bytes / 1024).toFixed(0)} KB`);
        }
        els['btn-export-folder'].classList.toggle('hidden', snapshot.stage !== 'done' || !snapshot.stats?.outputPath);
    }
    function schedule() {
        clearTimeout(timer);
        if (!disposed && state.running) timer = windowRef.setTimeout(() => refreshJob(false), 800);
    }
    async function refreshJob(adopt = false) {
        if (!state.sessionId || disposed) return;
        if (statusBusy) { statusAgain = {adopt}; return; }
        statusBusy = true;
        const captured = epoch, version = queryVersion, sessionId = state.sessionId;
        try {
            const snapshot = await invoke('gif_export_status', {sessionId, jobId: adopt ? null : state.jobId});
            if (captured !== epoch || version !== queryVersion || starting || disposed) return;
            if (state.accept(snapshot, adopt)) render();
        } catch (error) {
            if (captured === epoch && version === queryVersion && !disposed) hint(`读取导出状态失败，将重试：${normalizeError(error).message}`);
        } finally {
            statusBusy = false;
            if (statusAgain) { const next = statusAgain; statusAgain = null; refreshJob(next.adopt); }
            else schedule();
        }
    }
    async function refreshAvailability() {
        const version = ++availabilityVersion;
        try {
            const value = await invoke('gif_export_availability');
            if (version !== availabilityVersion || disposed) return;
            availability = value; availabilityError = '';
        } catch (error) {
            if (version !== availabilityVersion || disposed) return;
            availability = null; availabilityError = `媒体引擎状态不可用：${normalizeError(error).message}`;
        }
        render();
    }
    async function start() {
        if (busy() || !state.sessionId) return;
        if (format() === 'mp4' && availability?.state !== 'ready') { hint('请先在设置中安装或修复媒体引擎'); refreshAvailability(); return; }
        const path = els['export-path'].value.trim();
        if (!path) { hint('请先选择目标文件'); return; }
        const captured = epoch;
        const request = {sessionId: state.sessionId, path, format: format(), maxWidth: els['export-size'].value ? Number(els['export-size'].value) : null, quality: els['export-quality'].value};
        starting = true; queryVersion++; state.attach(null);
        els['btn-export-folder'].classList.add('hidden');
        els['export-progress'].value = 0; hint('创建导出任务…'); render();
        try {
            const id = await invoke('gif_begin_export', {request});
            if (captured !== epoch || disposed) return;
            state.attach(id);
        } catch (error) {
            if (captured !== epoch || disposed) return;
            const e = normalizeError(error); hint(`导出失败：${e.message}`);
            if (e.code === 'media_runtime_missing') refreshAvailability();
        } finally {
            if (captured === epoch && !disposed) {
                starting = false; render();
                if (state.jobId) refreshJob(false);
            }
        }
    }
    const open = () => {
        if (!els['export-dialog'].open) els['export-dialog'].showModal();
        refreshAvailability(); refreshJob(!state.running && !starting); render();
    };
    const pick = async () => {
        const captured = epoch, selected = format();
        const stamp = new Date().toISOString().replace(/[-:]/g, '').slice(0, 15).replace('T','-');
        try {
            const path = await saveDialog({title: `导出 ${selected.toUpperCase()}`, defaultPath: els['export-path'].value || `blink-${stamp}.${selected}`, filters: [{name: selected.toUpperCase(), extensions: [selected]}]});
            if (captured === epoch && format() === selected && !busy() && !disposed && path) els['export-path'].value = exportPath(path, selected);
        } catch (error) { if (captured === epoch && !disposed) hint(normalizeError(error).message); }
    };
    const change = () => { els['export-path'].value = exportPath(els['export-path'].value, format()); render(); refreshAvailability(); };
    const cancel = () => {
        const sessionId = state.sessionId, jobId = state.jobId, captured = epoch;
        if (!jobId || isTerminal(state.snapshot)) return;
        hint('正在取消并清理临时成品…');
        invoke('gif_cancel_export_job', {sessionId, jobId}).catch(error => { if (captured === epoch) hint(normalizeError(error).message); });
    };
    const install = () => invoke('open_settings_tab', {tab: 'media'}).catch(error => hint(normalizeError(error).message));
    const folder = () => {
        const path = state.snapshot?.stats?.outputPath;
        if (path) invoke('gif_open_containing_folder', {path}).catch(error => hint(normalizeError(error).message));
    };
    const close = () => els['export-dialog'].close();
    const focus = () => { if (els['export-dialog'].open) { refreshAvailability(); refreshJob(!state.running && !starting); } };
    const bindings = [['btn-export','click',open],['btn-pick-path','click',pick],['btn-export-confirm','click',start],
        ['export-format','change',change],['btn-export-cancel-req','click',cancel],['btn-export-install','click',install],
        ['btn-export-folder','click',folder],['btn-export-close','click',close]];
    bindings.forEach(([id,event,handler]) => els[id].addEventListener(event, handler));
    windowRef.addEventListener('focus', focus);
    listen('blink://gif-export-progress', event => {
        if (!disposed && !starting && state.accept(event.payload)) { render(); schedule(); }
    }).then(off => { if (disposed) off(); else unlisten = off; }).catch(error => console.warn('[media] 事件监听失败，将通过查询恢复', error));
    render();
    return {
        setSession(sessionId) {
            epoch++; queryVersion++; starting = false; clearTimeout(timer); state.reset(sessionId);
            els['export-path'].value = ''; els['export-progress-row'].classList.add('hidden'); els['btn-export-folder'].classList.add('hidden');
            if (els['export-dialog'].open) els['export-dialog'].close();
            render(); refreshJob(true);
        },
        dispose() {
            disposed = true; epoch++; clearTimeout(timer); unlisten?.();
            bindings.forEach(([id,event,handler]) => els[id].removeEventListener(event, handler));
            windowRef.removeEventListener('focus', focus);
        },
    };
}
