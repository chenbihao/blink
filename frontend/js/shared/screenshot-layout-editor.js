import {invoke, normalizeError} from './tauri.js';
import {saveConfig} from './config-keys.js';
import {DEFAULT_LAYOUT, normalizeLayout, moveLayoutAction, dropLayoutAction} from './screenshot-layout.js';
import {bindLayoutDrag} from './screenshot-layout-drag.js';
import {iconHTML} from './icon.js';
import {t} from '../i18n/index.js';

export function closeLayoutEditor({force = false} = {}) {
    const panel = document.getElementById('screenshot-layout-editor');
    if (!panel) return false;
    return panel._close(force) !== false;
}

export async function openLayoutEditor({position, returnFocus, onSaved = () => {}} = {}) {
    if (document.getElementById('screenshot-layout-editor')) return;
    const previousFocus = returnFocus || document.activeElement;
    const panel = document.createElement('section');
    panel.id = 'screenshot-layout-editor';
    panel.className = 'screenshot-layout-editor';
    if (position) panel.classList.add('ss-toolbar-surface');
    panel.setAttribute('role', 'dialog');
    panel.setAttribute('aria-label', t('ss.customize'));
    panel.tabIndex = -1;
    let live = true, saving = false, draft = normalizeLayout(), expected, drag;
    const close = (force = false) => {
        if (saving && !force) return false;
        live = false;
        drag?.cancel();
        panel.remove();
        previousFocus?.focus();
        return true;
    };
    panel._close = close;
    panel._reposition = () => { if (live) { drag?.cancel(); position?.(panel); } };
    panel.addEventListener('pointerdown', e => e.stopPropagation());
    panel.addEventListener('mousedown', e => e.stopPropagation());
    panel.addEventListener('keydown', e => {
        e.stopPropagation();
        if (e.key === 'Escape') { e.preventDefault(); if (!drag?.cancel()) close(); }
        if (e.key === 'Tab') {
            const items = [...panel.querySelectorAll('button:not(:disabled)')];
            const index = items.indexOf(document.activeElement);
            if (items.length) { e.preventDefault(); items[(index + (e.shiftKey ? -1 : 1) + items.length) % items.length].focus(); }
        }
    });
    const heading = document.createElement('header'); heading.className = 'ss-layout-header';
    const title = document.createElement('h3'); title.textContent = t('ss.customize');
    const hint = document.createElement('p'); hint.className = 'ss-layout-drag-hint'; hint.textContent = t('ss.dragHint');
    heading.append(title, hint);
    const lists = document.createElement('div');
    lists.className = 'ss-layout-lists';
    const status = document.createElement('div');
    status.className = 'ss-layout-status';
    status.setAttribute('role', 'status');
    const footer = document.createElement('div');
    footer.className = 'ss-layout-footer';
    const button = (label, run) => {
        const el = document.createElement('button');
        el.type = 'button'; el.textContent = label; el.onclick = run;
        return el;
    };
    const render = (focusId, focusControl) => {
        lists.replaceChildren();
        for (const zone of ['toolbar', 'more']) {
            const group = document.createElement('div');
            group.className = 'ss-layout-zone';
            group.dataset.zone = zone;
            const title = document.createElement('h4'); title.textContent = t(`ss.${zone}`); group.append(title);
            draft[zone].forEach((id, index) => {
                const row = document.createElement('div'); row.className = 'ss-layout-row sortable-item'; row.dataset.id = id;
                const grip = document.createElement('span'); grip.className = 'ss-layout-grip sortable-handle'; grip.setAttribute('aria-hidden', 'true'); grip.innerHTML = iconHTML('grip-vertical');
                const label = document.createElement('span'); label.className = 'ss-layout-label sortable-label'; label.textContent = t(`ss.action.${id}`); row.append(grip, label);
                const controls = [
                    [t('ss.up'), zone, index - 1, index === 0],
                    [t('ss.down'), zone, index + 1, index === draft[zone].length - 1],
                    [t(zone === 'toolbar' ? 'ss.toMore' : 'ss.toToolbar'), zone === 'toolbar' ? 'more' : 'toolbar', 999, false],
                ];
                controls.forEach(([label, target, slot, disabled], control) => {
                    const el = button(label, () => { if (saving) return; draft = moveLayoutAction(draft, id, target, slot); render(id, control); });
                    el.disabled = disabled; el.dataset.action = id; el.dataset.control = control;
                    row.append(el);
                });
                group.append(row);
            });
            lists.append(group);
        }
        panel.querySelector(`[data-action="${focusId}"][data-control="${focusControl ?? 2}"]`)?.focus();
        position?.(panel);
    };
    drag = bindLayoutDrag({panel, lists, isSaving: () => saving, onDrop: (id, zone, targetId, after) => {
        draft = dropLayoutAction(draft, id, zone, targetId, after);
        render(id);
    }});
    const save = button(t('ss.saveLayout'), async () => {
        if (saving) return;
        saving = true; save.disabled = true; cancel.disabled = true; reset.disabled = true;
        try {
            await saveConfig('screenshot_toolbar_layout', draft, {expected});
            if (!live) return;
            onSaved(normalizeLayout(draft)); saving = false; close();
        } catch (error) {
            if (live) status.textContent = normalizeError(error).code === 'config_conflict' ? t('ss.layoutConflict') : t('ss.saveFailed');
        } finally {
            saving = false; save.disabled = false; cancel.disabled = false; reset.disabled = false;
        }
    });
    const cancel = button(t('ss.cancel'), () => close());
    const reset = button(t('ss.defaults'), () => { draft = normalizeLayout(DEFAULT_LAYOUT); render(); });
    footer.append(reset, cancel, save); panel.append(heading, lists, status, footer);
    document.body.append(panel); save.disabled = true; panel.focus(); position?.(panel);
    try {
        const config = await invoke('get_config_section', {key: 'screenshot:config'});
        if (!live) return;
        expected = normalizeLayout(config?.toolbarLayout); draft = normalizeLayout(expected);
        render(); save.disabled = false;
    } catch { if (live) status.textContent = t('ss.loadFailed'); }
}
