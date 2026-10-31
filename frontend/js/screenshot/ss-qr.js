import {ss} from './ss-state.js';
import {IMAGE_SOURCE} from './image-editor-session.js';
import {cssRectToBitmap} from './ss-selection-geometry.js';
import {invoke, copyToClipboard} from '../shared/api.js';
import {normalizeError} from '../shared/tauri.js';
import {t} from '../i18n/index.js';
import {doCancel} from './ss-output.js';
import {showTransientHint} from './ss-ocr.js';
import {positionActionPanel} from './ss-action-menu.js';
import {findDisplayCssAt, getToolbarUiScale} from './ss-display.js';
import {clampPanelToMonitor} from './ss-panel-resize.js';
import {QrSession, createLatestQrResolver, qrActionAllowed} from './qr-session.js';

const session = new QrSession();
const classifyLatest = createLatestQrResolver(text => invoke('screenshot_qr_target', {text, execute: false}));
let panel = null;
const key = () => JSON.stringify([ss.editorSession.epoch, ss.selectionRevision, ss.editorSession.source, ss.selCss]);
export function qrPanelVisible() { return !!panel; }
export function closeQrPanel({focus = false} = {}) {
    if (!panel) return false;
    session.close();
    panel._disconnect?.();
    panel.remove();
    panel = null;
    if (focus) {
        const qr = document.getElementById('btn-qr');
        (qr?.classList.contains('ss-menu-action') ? document.getElementById('btn-more') : qr)?.focus();
    }
    return true;
}
export function resetQrPanel() { closeQrPanel(); session.reset(); }
ss._resetQrPanel = resetQrPanel;
ss._closeQrPanel = closeQrPanel;

function bindPanelGeometry(element, header) {
    header.addEventListener('pointerdown', e => {
        if (e.button !== 0 || e.target.closest('button')) return;
        e.preventDefault(); header.setPointerCapture(e.pointerId);
        const rect = element.getBoundingClientRect(); const start = {x:e.clientX,y:e.clientY,left:rect.left,top:rect.top};
        const move = event => {
            const mon = findDisplayCssAt(event.clientX,event.clientY); const scale = getToolbarUiScale();
            element.style.left = `${Math.max(mon.x+8,Math.min(start.left+event.clientX-start.x,mon.x+mon.w-element.offsetWidth*scale-8))}px`;
            element.style.top = `${Math.max(mon.y+8,Math.min(start.top+event.clientY-start.y,mon.y+mon.h-element.offsetHeight*scale-8))}px`;
        };
        const stop = () => { header.removeEventListener('pointermove',move); header.removeEventListener('pointerup',stop); header.removeEventListener('pointercancel',stop); };
        header.addEventListener('pointermove',move); header.addEventListener('pointerup',stop); header.addEventListener('pointercancel',stop);
    });
    // Native CSS resize uses layout pixels; clamp it to the target monitor after resize.
    const observer = new ResizeObserver(() => {
        if (!element.isConnected) { observer.disconnect(); return; }
        const left=parseFloat(element.style.left)||0, top=parseFloat(element.style.top)||0;
        const size = clampPanelToMonitor(left,top,element.offsetWidth,element.offsetHeight,getToolbarUiScale(),findDisplayCssAt(left,top));
        if (Math.abs(size.w-element.offsetWidth)>2) element.style.width=`${size.w}px`;
        if (Math.abs(size.h-element.offsetHeight)>2) element.style.height=`${size.h}px`;
    });
    observer.observe(element);
    element._disconnect = () => observer.disconnect();
}

export async function doQrSelection({fresh = false} = {}) {
    if (!ss.selCss || ss.sent) return;
    if (ss.editorSession.source === IMAGE_SOURCE.LONG_SCREENSHOT) {
        showTransientHint(t('ss.qr.unsupported'));
        return;
    }
    if (panel && !fresh) { closeQrPanel(); return; }
    closeQrPanel();
    ss.resultPanel = 'qr';
    document.getElementById('ocr-panel')?.remove();
    const frozenKey = key();
    const token = session.begin(frozenKey);
    const current = () => session.current(token,key()) && panel === element && !ss.sent;
    const element = document.createElement('section');
    element.id = 'qr-panel';
    element.className = 'ss-qr-panel';
    element.setAttribute('role','region'); element.setAttribute('aria-label',t('ss.action.qr'));
    panel=element;
    for (const name of ['pointerdown','mousedown','keydown']) element.addEventListener(name,e => e.stopPropagation());
    element.addEventListener('keydown',e => { if (e.key==='Escape' && !e.isComposing) { e.preventDefault(); closeQrPanel({focus:true}); } });
    const header = document.createElement('header');
    const title = document.createElement('span');
    title.textContent = t('ss.action.qr');
    const button = (label, run) => {
        const element = document.createElement('button');
        element.type = 'button';
        element.textContent = label;
        element.onclick = run;
        return element;
    };
    const close = button(t('ss.qr.close'), () => closeQrPanel({focus: true}));
    header.append(title, close);
    const text = document.createElement('textarea');
    text.readOnly = true;
    text.setAttribute('aria-label', t('ss.action.qr'));
    const status = document.createElement('div');
    status.className = 'ss-qr-status';
    status.setAttribute('role', 'status');
    status.textContent = t('ss.qr.loading');
    const footer=document.createElement('footer');
    element.append(header, status, text, footer);
    document.body.append(element);
    positionActionPanel(element);
    bindPanelGeometry(element, header);
    close.focus();
    const bmp = cssRectToBitmap(ss.selCss, window.__blinkScreenMeta || {vx:0,vy:0});
    let results;
    try {
        results = !fresh && session.cache?.key === frozenKey ? session.cache.results : await invoke('screenshot_decode_qr',{
            source:ss.editorSession.canUseCaptureCropFastPath?'screenshot':'editor',x:bmp.x,y:bmp.y,w:bmp.w,h:bmp.h,
        });
        if (!current()) return;
        session.cache={key:frozenKey,results};
    } catch (error) {
        if (current()) {
            const code = normalizeError(error).code;
            const message = {resource_limit: 'limit', timeout: 'timeout', busy: 'busy'}[code] || 'failed';
            status.textContent = t(`ss.qr.${message}`);
            footer.append(button(t('ss.qr.retry'), () => doQrSelection({fresh: true})));
        }
        return;
    }
    if (!results.length) {
        status.textContent = t('ss.qr.empty');
        footer.append(button(t('ss.qr.retry'), () => doQrSelection({fresh: true})));
        return;
    }
    let index = 0, itemGeneration = 0, opening = false;
    const render = async () => {
        const item = results[index];
        const revision = ++itemGeneration;
        text.value = item;
        status.textContent = t('ss.qr.count', {index: index + 1, count: results.length});
        footer.replaceChildren();
        const copy = button(t('ss.qr.copy'), async () => {
            try {
                await copyToClipboard(item);
                if (current() && revision === itemGeneration) copy.textContent = t('ss.qr.copied');
            } catch {
                if (current()) status.textContent = t('ss.qr.openFailed');
            }
        });
        footer.append(copy,button(t('ss.qr.retry'),()=>doQrSelection({fresh:true})));
        if(results.length>1) {
            const select = document.createElement('select');
            select.setAttribute('aria-label', t('ss.action.qr'));
            results.forEach((_, i) => {
                const option = document.createElement('option');
                option.value = i;
                option.textContent = t('ss.qr.count', {index: i + 1, count: results.length});
                select.append(option);
            });
            select.value = index;
            select.onchange = () => {
                if (opening) return;
                index = Number(select.value);
                void render();
            };
            footer.prepend(select);
        }
        let target;
        try { target=await classifyLatest(item, () => current() && revision === itemGeneration); } catch { return; }
        if(!current()||revision!==itemGeneration) return;
        if(!qrActionAllowed(target)) { if(target?.kind==='unavailable') status.textContent=t('ss.qr.unavailable'); return; }
        if(target.kind==='web') {
            status.textContent += ` · ${target.host}`;
        }
        const open = button(t(`ss.qr.${target.kind}`), async () => {
            if (opening || !current()) return;
            opening = true;
            const setDisabled = disabled => {
                for (const control of footer.querySelectorAll('button,select')) control.disabled = disabled;
            };
            setDisabled(true);
            try {
                await invoke('screenshot_qr_target', {text: item, execute: true});
                if (current()) doCancel();
            } catch {
                if (current()) status.textContent = t('ss.qr.openFailed');
            } finally {
                opening = false;
                if (current()) setDisabled(false);
            }
        });
        footer.append(open);
    };
    void render();
}
