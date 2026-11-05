//! 录制控件与浮层：统一逻辑值、自绘下拉、缩放和屏内定位。
import {t, onLangChange} from '../i18n/index.js';
import {ss} from './ss-state.js';
import {recordingSession as state} from './recording-session.js';
import {applyFloatingUiScale, findDisplayCssAt, positionToolbar} from './ss-display.js';
import {bindRecordingHoverMenu} from './recording-hover-menu.js';

const $ = id => document.getElementById(id);
let opened = null, changed = () => {}, bound = false;
let panelLayoutFrame = 0, panelObserver = null;
let finishMenu = null;
const selects = new Map();
export function nextRecordingValue(values, current, step = 1, wrap = true) {
    if (!values.length) return current;
    const index = values.indexOf(current), next = index < 0 ? 0 : index + step;
    return values[wrap ? (next + values.length) % values.length : Math.max(0, Math.min(values.length - 1, next))];
}
export function bindRecordingWheel(control, step, enabled) {
    control?.addEventListener('wheel', event => {
        if (!event.deltaY || !enabled()) return;
        event.preventDefault(); event.stopPropagation();
        step(event.deltaY > 0 ? 1 : -1);
    }, {passive: false});
}
export function formatRecordingBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes < 0) return '—';
    const unit = bytes >= 1024 ** 3 ? 'GB' : 'MB';
    return `${(bytes / (unit === 'GB' ? 1024 ** 3 : 1024 ** 2)).toFixed(1)} ${unit}`;
}

/** data-mode 是唯一显隐依据；每次渲染完整推导 disabled，绝不继承上一轮禁用状态。 */
export function renderRecordingUi() {
    const toolbar = ss.toolbar;
    const layoutChanged = toolbar && (toolbar.dataset.mode !== state.mode || toolbar.dataset.drawing !== String(state.drawing));
    if (layoutChanged) retainToolbarAnchor();
    if (toolbar) {
        toolbar.dataset.mode = state.mode;
        toolbar.dataset.drawing = String(state.drawing);
        toolbar.setAttribute('aria-label', t(state.visible ? 'gif.rec.recordingToolbar' : 'gif.rec.screenshotToolbar'));
    }
    document.body.classList.toggle('gif-recording', state.visible);
    document.documentElement.classList.toggle('gif-recording', state.visible);
    document.body.classList.toggle('gif-rec-paused', state.mode === 'paused');
    const label = t('gif.rec.' + ({screenshot:'ready',ready:'ready',starting:'startingShort',recording:'recording',paused:'paused',stopping:'finishing'}[state.mode]));
    const toggle = $('gif-record-toggle');
    if (toggle) {
        toggle.disabled = state.busy;
        const key = {screenshot:'startShort',ready:'startShort',starting:'startingShort',recording:'pauseShort',paused:'resumeShort',stopping:'finishing'}[state.mode];
        $('gif-record-toggle-label').textContent = t('gif.rec.' + key);
        toggle.title = t('gif.rec.' + ({ready:'start',recording:'pause',paused:'resume'}[state.mode] || key));
        toggle.setAttribute('aria-label', toggle.title);
        const icons = toggle.querySelectorAll('.icon');
        icons[0]?.classList.toggle('hidden', state.mode !== 'recording');
        icons[1]?.classList.toggle('hidden', !['ready','starting','paused'].includes(state.mode));
    }
    const stop = $('gif-rec-stop');
    if (stop) {
        stop.hidden = !state.active; stop.disabled = state.busy; stop.title = t('gif.rec.finish'); stop.setAttribute('aria-label',stop.title);
        if($('gif-rec-stop-label')) $('gif-rec-stop-label').textContent=stop.title;
    }
    const exit = $('gif-record-exit');
    if ($('gif-region-frame')) $('gif-region-frame').dataset.resizable = String(state.preparing);
    if (exit) {
        exit.hidden = state.active;
        exit.disabled = state.busy;
        exit.title = t('gif.rec.exitMode');
        exit.setAttribute('aria-label',exit.title);
        if($('gif-record-exit-label')) $('gif-record-exit-label').textContent=t('gif.rec.exitShort');
    }
    const draw = $('gif-rec-draw');
    if (draw) {
        draw.disabled = state.busy || state.restoredControlsOnly;
        draw.title = t(state.restoredControlsOnly ? 'gif.rec.restoreDrawNote' : state.drawing ? 'gif.rec.drawCollapse' : 'gif.rec.drawExpand');
        draw.setAttribute('aria-label',draw.title);
        draw.setAttribute('aria-pressed', String(state.drawing));
        draw.setAttribute('aria-expanded', String(state.drawing));
        draw.querySelector('.recording-draw-chevron use')?.setAttribute('href', state.drawing ? '#icon-chevron-left' : '#icon-chevron-right');
        draw.classList.toggle('active',state.drawing);
    }
    if ($('gif-audio-open')) $('gif-audio-open').disabled = state.busy || !state.preparing;
    if ($('gif-audio-menu')) $('gif-audio-menu').disabled = state.busy;
    const recent = $('gif-recording-recent');
    if (recent) {
        // 进入最近录制会退出整个截图面，采集进行中不得切走。
        const busy = state.busy || !state.preparing;
        recent.disabled = busy;
        recent.title = t(busy ? 'gif.rec.recentBusy' : 'gif.rec.recentEntryTitle');
        recent.setAttribute('aria-label', recent.title);
    }
    const fps = $('gif-prep-fps');
    if (fps) { fps.disabled = !state.preparing; if(state.active) fps.value=String(state.targetFps); syncRecordingSelect('gif-prep-fps'); }
    if (!state.active && $('gif-rec-time')) $('gif-rec-time').textContent='00:00';
    const storage = state.storage || {};
    const status = $('gif-recording-status');
    if (status) {
        const space = storage.disk ? t('gif.rec.available',{size:formatRecordingBytes(storage.disk.availableBytes)}) : t('gif.rec.spaceUnknown');
        const title = `${t('gif.rec.details')} · ${label}\n${space}`;
        if (status.title !== title) status.title = title;
        status.setAttribute('aria-label',title);
        status.classList.toggle('warn',!!storage.warning || !!storage.pauseReason);
    }
    const discard = $('gif-rec-discard');
    if (discard) { discard.disabled = !state.active || state.busy; discard.textContent = t('gif.rec.discardAction'); }
    finishMenu?.sync();
    const list = $('gif-recording-details-values');
    if (list) {
        list.replaceChildren();
        const rows = [
            [t('gif.rec.detailsState'), label],
            [t('gif.rec.detailsMaterial'),formatRecordingBytes(storage.materialBytes)],
            [t('gif.rec.detailsDisk'),storage.disk ? `${storage.disk.volume} · ${formatRecordingBytes(storage.disk.availableBytes)}` : t('gif.rec.spaceUnknown')],
            [t('gif.rec.detailsFps'),`${state.targetFps} / ${Number.isFinite(state.actualFps) ? state.actualFps.toFixed(1) : '—'} fps`],
        ];
        for(const [key,value] of rows) {const dt=document.createElement('dt'),dd=document.createElement('dd');dt.textContent=key;dd.textContent=value;list.append(dt,dd);}
    }
    ss._syncRecordingAudio?.();
    const grip=$('toolbar-drag');
    if(grip) {grip.title=t('ss.toolbar.drag');grip.setAttribute('aria-label',grip.title);}
    const move=$('gif-rec-move');
    if(move) {move.disabled=state.busy;move.title=t('gif.rec.moveTogether');move.setAttribute('aria-label',move.title);}
    if (layoutChanged && state.visible) ss._syncRecordingToolbar?.();
}

export function setRecordingUiListener(listener) { changed = listener; }
export function recordingUiChanged() { if (opened) reflowRecordingPanels(); changed(); }

export function showRecordingMessage(message) {
    const panel = $('gif-recording-error');
    if (!panel) return;
    panel.textContent = message || '';
    panel.classList.toggle('hidden', !message);
    if (message) reflowRecordingPanels();
    changed();
}

/** 面切换前保存左侧抓手锚点，尺寸变化后只向屏内钳制。 */
export function retainToolbarAnchor() {
    const toolbar = ss.toolbar;
    if (!toolbar || toolbar.classList.contains('hidden')) return;
    toolbar.dataset.keepAnchor = 'true';
}

export function positionRecordingPanel(panel, anchor) {
    if (!panel || !anchor) return;
    const scale = applyFloatingUiScale(panel);
    const rect = (anchor.closest?.('.recording-split') || anchor).getBoundingClientRect();
    const mon = findDisplayCssAt(rect.left + rect.width / 2, rect.top + rect.height / 2);
    panel.style.maxWidth = `${Math.max(1, mon.w - 16) / scale}px`;
    panel.style.maxHeight = `${Math.max(1, Math.floor(recordingPanelHeightLimit(rect, mon, 6 * scale) / scale))}px`;
    const placed = placeRecordingPanel(rect, panel.offsetWidth * scale, panel.offsetHeight * scale, mon, 6 * scale);
    panel.style.left = `${placed.left}px`;
    panel.style.top = `${placed.top}px`;
}

export function recordingPanelHeightLimit(anchor, monitor, gap = 6) {
    const bottom = anchor.bottom ?? anchor.top + anchor.height;
    return Math.max(1, monitor.y + monitor.h - 8 - bottom - gap, anchor.top - gap - monitor.y - 8);
}

/** 设置浮层只锚定按钮和屏幕边界，不因录制选区而远离触发点。 */
export function placeRecordingPanel(anchor, width, height, monitor, gap = 6) {
    const margin = 8;
    const minX = monitor.x + margin, minY = monitor.y + margin;
    const maxX = Math.max(minX, monitor.x + monitor.w - width - margin);
    const maxY = Math.max(minY, monitor.y + monitor.h - height - margin);
    const bottom = anchor.bottom ?? anchor.top + anchor.height;
    const below = bottom + gap, above = anchor.top - height - gap;
    const preferBelow = below + height <= monitor.y + monitor.h - margin || above < minY;
    return {left: Math.max(minX, Math.min(anchor.left, maxX)),
        top: Math.max(minY, Math.min(preferBelow ? below : above, maxY))};
}

export function closeRecordingPanel({focus = false} = {}) {
    if (!opened) return false;
    const previous = opened;
    opened = null;
    panelObserver?.unobserve(previous.panel);
    previous.panel.classList.add('hidden');
    previous.anchor.setAttribute('aria-expanded', 'false');
    previous.onClose?.({restoreFocus: focus});
    if (previous.requestFocus) ss.gifRecHooks?.onPanelFocus?.(false);
    if (focus) previous.anchor.focus({preventScroll: true});
    changed();
    return true;
}

export function openRecordingPanel(panel, anchor, onClose, {toggle = true, requestFocus = true} = {}) {
    if (opened?.panel === panel) {
        if (toggle) { closeRecordingPanel(); return false; }
        if (requestFocus && !opened.requestFocus) { opened.requestFocus = true; ss.gifRecHooks?.onPanelFocus?.(true); }
        return true;
    }
    closeRecordingPanel();
    ss._closeToolDropdowns?.();
    ss._closeActionMenu?.();
    panel.classList.remove('hidden');
    anchor.setAttribute('aria-expanded', 'true');
    opened = {panel, anchor, onClose, requestFocus};
    if (!panelObserver && typeof ResizeObserver === 'function') panelObserver = new ResizeObserver(reflowRecordingPanels);
    panelObserver?.observe(panel);
    if (requestFocus) ss.gifRecHooks?.onPanelFocus?.(true);
    reflowRecordingPanels();
    changed();
    return true;
}

export function reflowRecordingPanels() {
    if (panelLayoutFrame) return;
    panelLayoutFrame = requestAnimationFrame(() => {
        panelLayoutFrame = 0;
        if (opened) positionRecordingPanel(opened.panel, opened.anchor);
        const error = $('gif-recording-error');
        if (error && !error.classList.contains('hidden')) positionRecordingPanel(error, ss.toolbar);
        changed();
    });
}

export function setRecordingSelectOptions(id, options, value = $(id)?.value) {
    const select = selects.get(id);
    if (!select) return;
    select.options = options;
    const input = $(id);
    input.value = options.some(o => o.value === value) ? value : (options[0]?.value ?? '');
    const current = options.find(o => o.value === input.value);
    select.trigger.querySelector('.recording-select-label').textContent = current?.label || '';
    select.panel.replaceChildren();
    for (const option of options) {
        const button = document.createElement('button');
        button.type = 'button'; button.className = 'recording-option';
        button.setAttribute('role', 'option');
        button.setAttribute('aria-selected', String(option.value === input.value));
        button.textContent = option.label;
        button.addEventListener('click', () => {
            if (input.disabled) return;
            const different = input.value !== option.value;
            input.value = option.value;
            closeRecordingPanel({focus: true});
            setRecordingSelectOptions(id, select.options, option.value);
            if (different) input.dispatchEvent(new Event('change', {bubbles: true}));
        });
        select.panel.append(button);
    }
}

export function syncRecordingSelect(id) {
    const select = selects.get(id);
    if (!select) return;
    select.trigger.disabled = !!$(id).disabled;
    select.menuToggle.disabled = !!$(id).disabled;
    const value = $(id).value;
    const current = select.options.find(o => o.value === value);
    select.trigger.querySelector('.recording-select-label').textContent = current?.label || '';
    if (id === 'gif-prep-fps') {
        select.trigger.title = `${t('gif.rec.fps')}：${current?.label || ''}${$(id).disabled ? '' : ' · ' + t('gif.rec.clickCycle')}`;
        select.trigger.setAttribute('aria-label', select.trigger.title);
        select.menuToggle.title = t('gif.rec.fpsOptions');
        select.menuToggle.setAttribute('aria-label', select.menuToggle.title);
    }
    for (const [i, button] of [...select.panel.children].entries()) {
        button.setAttribute('aria-selected', String(select.options[i]?.value === value));
    }
}

export function bindRecordingControls() {
    if (bound) return;
    bound = true;
    ss._reflowRecordingPanels = reflowRecordingPanels;
    const fitToolbar = () => {
        const toolbar = ss.toolbar;
        if (!ss.selCss || !toolbar || toolbar.classList.contains('hidden') ||
            !state.visible) return;
        retainToolbarAnchor();
        positionToolbar({...ss.selCss}, true);
    };
    ss._syncRecordingToolbar = fitToolbar;
    const finishPanel = $('gif-recording-finish-menu'), stop = $('gif-rec-stop');
    finishMenu = bindRecordingHoverMenu({trigger: stop, panel: finishPanel,
        enabled: () => state.active && !state.busy,
        isOpen: () => opened?.panel === finishPanel,
        open: keyboard => openRecordingPanel(finishPanel, stop, ({restoreFocus}) => finishMenu.dismiss(restoreFocus), {toggle: false, requestFocus: keyboard}),
        close: () => { if (opened?.panel === finishPanel) closeRecordingPanel(); },
    });
    // 设备发现/语言切换可能在切面完成后改变栏宽，不能只在首次开栏时测量。
    if (typeof ResizeObserver === 'function' && ss.toolbar) new ResizeObserver(fitToolbar).observe(ss.toolbar);
    for (const trigger of document.querySelectorAll('[data-recording-select]')) {
        const id = trigger.dataset.recordingSelect;
        const menuToggle = document.querySelector(`[data-recording-menu="${id}"]`);
        const panel = document.createElement('div');
        panel.id = `${id}-options`; panel.className = 'recording-panel recording-options hidden';
        panel.setAttribute('role', 'listbox'); panel.setAttribute('aria-label', trigger.getAttribute('aria-label') || '');
        document.body.append(panel);
        menuToggle.setAttribute('aria-controls', panel.id); menuToggle.setAttribute('aria-haspopup', 'listbox');
        menuToggle.setAttribute('aria-expanded', 'false');
        selects.set(id, {trigger, menuToggle, panel, options: []});
        const cycle = (step = 1, wrap = true) => {
            if ($(id).disabled) return;
            closeRecordingPanel();
            const values = selects.get(id).options.map(option => option.value);
            const next = nextRecordingValue(values, $(id).value, step, wrap);
            if (next === $(id).value) return;
            $(id).value = next;
            syncRecordingSelect(id);
            $(id).dispatchEvent(new Event('change', {bubbles: true}));
        };
        trigger.addEventListener('click', () => cycle());
        bindRecordingWheel(trigger.closest('.recording-split'), step => cycle(step, false), () => !$(id).disabled);
        menuToggle.addEventListener('click', () => {
            if ($(id).disabled) return;
            if (openRecordingPanel(panel, menuToggle)) panel.querySelector('[aria-selected="true"]')?.focus({preventScroll: true});
        });
        trigger.addEventListener('keydown', event => {
            if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key) || $(id).disabled) return;
            event.preventDefault();
            if (opened?.panel !== panel) openRecordingPanel(panel, menuToggle);
            const items = [...panel.children];
            (event.key === 'End' || event.key === 'ArrowUp' ? items.at(-1) : items[0])?.focus({preventScroll: true});
        });
        panel.addEventListener('keydown', event => {
            const items = [...panel.children], i = items.indexOf(document.activeElement);
            if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
            event.preventDefault();
            const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 :
                (i + (event.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length;
            items[next]?.focus({preventScroll: true});
        });
    }
    const localize = () => {
        setRecordingSelectOptions('gif-prep-fps', [10, 15, 24, 30, 60].map(n => ({value: String(n), label: `${n} fps`})));
        setRecordingSelectOptions('gif-audio-mode', [
            {value: 'silent', label: t('gif.rec.silent')}, {value: 'microphone', label: t('gif.audio.microphone')},
            {value: 'system', label: t('gif.audio.system')}, {value: 'both', label: t('gif.rec.both')},
        ]);
        const labels = {'gif-recording-status':'details'};
        for (const [id,key] of Object.entries(labels)) {
            const node = $(id); if (!node) continue;
            node.title = t(`gif.rec.${key}`); node.setAttribute('aria-label', node.title);
        }
        const fps = selects.get('gif-prep-fps');
        fps.trigger.closest('.recording-split')?.setAttribute('aria-label', t('gif.rec.fps'));
        fps.trigger.setAttribute('aria-label', t('gif.rec.fps'));
        fps.panel.setAttribute('aria-label', t('gif.rec.fps'));
        syncRecordingSelect('gif-prep-fps');
        selects.get('gif-audio-mode')?.panel.setAttribute('aria-label', t('gif.rec.sound'));
        for (const node of document.querySelectorAll('[data-recording-text]')) node.textContent = t(node.dataset.recordingText);
    };
    localize(); onLangChange(() => {localize(); renderRecordingUi();});
    $('gif-recording-status')?.addEventListener('click', () => openRecordingPanel($('gif-recording-details'), $('gif-recording-status')));
    document.addEventListener('pointerdown', event => {
        if (opened && !opened.panel.contains(event.target) && !opened.anchor.contains(event.target)) closeRecordingPanel();
    }, true);
    window.addEventListener('keydown', event => {
        if (event.key !== 'Escape' || event.isComposing || !opened) return;
        event.preventDefault(); event.stopImmediatePropagation();
        closeRecordingPanel({focus: true});
    }, true);
    window.addEventListener('resize', reflowRecordingPanels);
}
