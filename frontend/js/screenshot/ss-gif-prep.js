//! 录制准备与启动：模式由 recording-session 唯一持有，按钮状态只由渲染投影。
import {t} from '../i18n/index.js';
import {ss} from './ss-state.js';
import {recordingSession as state} from './recording-session.js';
import {bindRecordingControls, retainToolbarAnchor, closeRecordingPanel, showRecordingMessage, renderRecordingUi} from './ss-recording-ui.js';
import {cssPointToScreen, cssRectToBitmap} from './ss-selection-geometry.js';
import {positionToolbar} from './ss-display.js';
import {invoke, normalizeError} from '../shared/tauri.js';
import {showTransientHint} from './ss-ocr.js';
import {annotationsMigratable, enterGifRecordOverlay, handleOverlayHidden, prepareRecordingTools, waitForRecordingPreparation, restorePreparationAnnotations, commitRecordingSelection} from './ss-gif-rec.js';
import {drawStaticBase, drawFinalSelection} from './ss-draw.js';
import {audioOptions, enterAudioPrep, stopAudioMonitor, bindAudioControls} from './ss-gif-audio.js';

export function isGifPrepActive() { return state.mode === 'ready' || (state.mode === 'starting' && !state.active); }
export function isRecordingStarting() { return state.mode === 'starting'; }

function regionSizeValid() {
    if (!ss.selCss) return false;
    const bmp=cssRectToBitmap(ss.selCss,window.__blinkScreenMeta || {vx:0,vy:0});
    return bmp.w>=64 && bmp.h>=64 && bmp.w*bmp.h*4<=64*1024*1024;
}

export function enterGifRecordMode() {
    if (!ss.toolbar || state.visible) return;
    if (!regionSizeValid()) {
        showTransientHint(t('gif.rec.invalidRegion'),{isError:true,duration:3000}); return;
    }
    if (!annotationsMigratable()) return;
    state.reset('ready');
    retainToolbarAnchor(); closeRecordingPanel(); showRecordingMessage('');
    ss._closeActionMenu?.(); ss._closeLayoutEditor?.();
    ss.sent=false;
    const epoch=state.epoch;
    void prepareRecordingTools().catch(error=>{
        if(!state.owns(epoch,null)) return;
        exitGifRecordMode();
        showTransientHint(t('gif.rec.startFailed',{message:normalizeError(error).message}),{isError:true,duration:3000});
    });
    renderRecordingUi(); enterAudioPrep();
    invoke('gif_storage_status').then(disk=>{
        if (!state.owns(epoch,null) || !state.preparing) return;
        state.storage={disk}; renderRecordingUi();
    }).catch(()=>{});
    positionToolbar({...ss.selCss},true);
}

export function exitGifRecordMode() {
    if (!state.preparing) return;
    retainToolbarAnchor();
    const closing = handleOverlayHidden();
    drawStaticBase();drawFinalSelection();
    restorePreparationAnnotations();
    ss.sent=false;
    ss.toolbar?.classList.remove('hidden');
    if(ss.selCss) positionToolbar({...ss.selCss},true);
    document.getElementById('btn-gif')?.focus({preventScroll:true});
    return closing;
}

/** 原生准备租约归还后才能隐藏截图；截图确实收起后再打开编辑器。 */
async function openRecentRecordings() {
    if (!state.preparing || state.busy || ss.cancelInProgress) return;
    const generation = ss._loadGen;
    const closing = exitGifRecordMode();
    const epoch = state.epoch;
    const current = () => ss._loadGen === generation && state.epoch === epoch && !state.visible;
    try {
        await closing;
        if (!current()) return;
        if (!await ss._doCancel?.()) return;
        if (!current()) return;
        await invoke('gif_open_editor_home');
    } catch (error) {
        console.error('[recording] 打开最近录制失败', error);
        if (current()) showTransientHint(t('gif.rec.recentFailed', {message:normalizeError(error).message}), {isError:true,duration:3000});
    }
}

export async function startGifRecordingFromPrep() {
    if (!state.preparing || ss.sent) return;
    commitRecordingSelection();
    if (!regionSizeValid()) {showTransientHint(t('gif.rec.invalidRegion'),{isError:true,duration:3000});return;}
    if (!annotationsMigratable()) return;
    const epoch=state.epoch;
    const fps=Number(document.getElementById('gif-prep-fps').value)||15;
    const audio=audioOptions();
    const meta=window.__blinkScreenMeta || {vx:0,vy:0};
    const origin=cssPointToScreen(ss.selCss.x,ss.selCss.y,meta),bmp=cssRectToBitmap(ss.selCss,meta);
    state.setMode('starting'); ss.sent=true;
    closeRecordingPanel(); renderRecordingUi();
    let sessionId=null;
    try {
        await waitForRecordingPreparation();
        await stopAudioMonitor();
        if(!state.owns(epoch,null)) return;
        const info=await invoke('gif_start_recording',{x:origin.x,y:origin.y,w:bmp.w,h:bmp.h,fps,showCursor:true,audio});
        sessionId=info.sessionId;
        if(!state.owns(epoch,null)) {await invoke('gif_cancel_recording',{sessionId});return;}
        await enterGifRecordOverlay({x:origin.x,y:origin.y,w:bmp.w,h:bmp.h},sessionId);
    } catch(error) {
        if(state.epoch!==epoch) return;
        if(sessionId) await invoke('gif_cancel_recording',{sessionId}).catch(()=>{});
        handleOverlayHidden();
        drawStaticBase();drawFinalSelection();
        restorePreparationAnnotations();
        ss.sent=false;
        console.error('[recording] startup failed',error);
        showTransientHint(t('gif.rec.startFailed',{message:normalizeError(error).message}),{isError:true,duration:3000});
        // 原底图尚在时允许重试；已创建采集会话的失败留给新截图建立新底图。
        if(!sessionId) enterGifRecordMode();
    }
}

export function bindGifPrepBar() {
    bindRecordingControls(); bindAudioControls();
    ss._resetRecordingUi=()=>{if(state.visible) handleOverlayHidden();};
    ss._recordingPreventsCleanup=()=>state.visible;
    window.addEventListener('keydown',event=>{
        if(event.key!=='Escape'||event.isComposing||!state.preparing) return;
        event.preventDefault();event.stopImmediatePropagation();exitGifRecordMode();
    },true);
    document.getElementById('gif-record-toggle')?.addEventListener('click',()=>{if(state.preparing) void startGifRecordingFromPrep();});
    document.getElementById('gif-record-exit')?.addEventListener('click',()=>{if(state.preparing) exitGifRecordMode();});
    document.getElementById('gif-recording-recent')?.addEventListener('click',()=>{void openRecentRecordings();});
    renderRecordingUi();
}
