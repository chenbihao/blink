import {invoke, listen, confirmDialog, normalizeError} from '../../../shared/tauri.js';
import {EVENTS} from '../../../shared/event-names.js';
import {onLangChange, t} from '../../../i18n/index.js';
import {formatBytes, progressPercent} from '../../../shared/download-progress.js';
import {mergeMediaRuntimeSnapshot, mediaRuntimeAction} from './media-runtime-state.js';

let initialized = false;
let snapshot = null;
let refreshing = false;
let refreshAgain = false;
let starting = false;
let cancellingId = null;
let localError = '';
let statusError = '';
const el = id => document.getElementById(`media-runtime-${id}`);

function render() {
    const runtime = snapshot?.runtime;
    const operation = snapshot?.operation;
    const busy = starting || !!operation;
    const state = runtime?.state || 'loading';
    const badge = el('status');
    if (!badge) return;
    badge.textContent = t(`engine.media.${operation ? 'stage.' + operation.stage : 'state.' + state}`);
    badge.className = 'status-badge ' + (operation ? 'status-unknown' : state === 'ready' ? 'status-available'
        : state === 'corrupt' ? 'status-unavailable' : 'status-unknown');
    const action = mediaRuntimeAction(snapshot);
    el('action').textContent = t(`engine.media.action.${action || 'install'}`);
    el('action').disabled = busy || !action;
    // 0.28.27：「测试可用性」（diagnose）仅随设置页既有调试模式显示——只隐藏
    // 测试动作本身，安装/修复主按钮不受影响（ready 态侧栏「修复」按钮恒在，
    // 普通模式不丢失手动修复入口）。状态轮询只读，不因显隐启动自检。
    const diagnoseHidden = action === 'diagnose'
        && document.documentElement?.dataset?.developerMode !== 'true';
    el('action').hidden = state === 'pending_remove' || diagnoseHidden;
    el('repair').hidden = !runtime || state !== 'ready';
    el('repair').disabled = busy;
    el('uninstall').hidden = !runtime || state === 'missing';
    el('uninstall').disabled = busy;
    el('cancel').hidden = !operation?.cancellable;
    el('cancel').disabled = !operation?.cancellable || cancellingId === operation?.id;
    if (!operation) cancellingId = null;
    el('detail').textContent = runtime?.version
        ? t('engine.media.installed_detail', {version: runtime.version, size: formatBytes(runtime.installedBytes)})
        : runtime ? t('engine.media.download_detail', {version: runtime.targetVersion, size: formatBytes(runtime.downloadBytes)}) : '';
    el('error').textContent = localError || statusError || snapshot?.lastError || runtime?.message || '';
    el('error').hidden = !el('error').textContent;
    const progress = el('progress');
    progress.hidden = !operation;
    if (operation) {
        const percent = operation.stage === 'downloading' ? progressPercent(operation.downloaded, operation.total) : null;
        const fill = progress.querySelector('.download-progress__fill');
        fill.classList.toggle('download-progress__fill--indeterminate', percent === null);
        fill.style.width = percent === null ? '' : `${percent}%`;
        progress.querySelector('.download-progress__text').textContent = operation.stage === 'downloading'
            ? `${formatBytes(operation.downloaded)} / ${formatBytes(operation.total ?? runtime.downloadBytes)}`
            : t(`engine.media.stage.${operation.stage}`);
    }
}

async function refresh() {
    if (refreshing) { refreshAgain = true; return; }
    refreshing = true;
    try {
        do {
            refreshAgain = false;
            snapshot = mergeMediaRuntimeSnapshot(snapshot, await invoke('get_ffmpeg_runtime_status'));
            statusError = '';
            render();
        } while (refreshAgain);
    } catch (error) {
        statusError = normalizeError(error).message;
        render();
    } finally { refreshing = false; }
}

async function begin(action) {
    if (starting || snapshot?.operation || !action) return;
    starting = true;
    localError = '';
    render();
    try {
        await invoke('begin_ffmpeg_runtime_operation', {action});
    } catch (error) { localError = normalizeError(error).message; }
    finally { starting = false; await refresh(); }
}

export function initMediaRuntime() {
    if (initialized) return;
    initialized = true;
    el('action')?.addEventListener('click', () => void begin(mediaRuntimeAction(snapshot)));
    el('repair')?.addEventListener('click', () => void begin('repair'));
    el('uninstall')?.addEventListener('click', async () => {
        if (starting || snapshot?.operation) return;
        if (await confirmDialog(t('engine.media.uninstall_confirm'), {title: t('engine.media.action.uninstall'), kind: 'warning'})) {
            await begin('uninstall');
        }
    });
    el('cancel')?.addEventListener('click', async () => {
        const id = snapshot?.operation?.id;
        if (!id) return;
        cancellingId = id;
        render();
        try { await invoke('cancel_ffmpeg_runtime_operation', {operationId: id}); }
        catch (error) { localError = normalizeError(error).message; cancellingId = null; }
        await refresh();
    });
    // 通知只触发拉取，不直接应用事件内的阶段，避免旧操作事件回退卡片。
    listen(EVENTS.MANAGED_RUNTIME_INSTALL, event => {
        if (event.payload?.kind === 'ffmpeg') void refresh();
    }).catch(error => { console.error('[media-runtime] 状态监听失败，保留轮询', normalizeError(error)); });
    onLangChange(render);
    // 0.28.27：调试模式切换（data-developer-mode 变化）时重渲染，测试动作
    // 显隐即时生效，普通模式下不重新显示。
    if (typeof MutationObserver === 'function' && document.documentElement) {
        try {
            new MutationObserver(render).observe(document.documentElement, {
                attributes: true, attributeFilter: ['data-developer-mode'],
            });
        } catch (error) {
            console.warn('[media-runtime] 调试模式观察失败，显隐以刷新时点为准', error);
        }
    }
    setInterval(() => {
        if (!document.hidden && document.querySelector('#managed-engines.active')) void refresh();
    }, 3000);
    document.addEventListener('visibilitychange', () => { if (!document.hidden) void refresh(); });
    void refresh();
}
