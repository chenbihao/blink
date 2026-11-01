import {ss} from './ss-state.js';
import {ACTION_IDS, normalizeLayout, fitLayout} from '../shared/screenshot-layout.js';
import {openLayoutEditor, closeLayoutEditor} from '../shared/screenshot-layout-editor.js';
import {applyFloatingUiScale, findDisplayCssAt} from './ss-display.js';
import {placeActionMenu} from './ss-panel-resize.js';
import {t, applyI18nFromConfig} from '../i18n/index.js';
import {listen} from '../shared/tauri.js';
import {EVENTS} from '../shared/event-names.js';

let widthBudget = Infinity;
let appliedLayoutKey = null;
let menu, trigger, group, buttons, layout = normalizeLayout();
export function closeActionMenu({focus = false} = {}) {
    if (!menu || menu.hidden) return false;
    menu.hidden = true; trigger.setAttribute('aria-expanded', 'false');
    if (focus) trigger.focus();
    return true;
}
export function positionActionPanel(panel) {
    const scale = applyFloatingUiScale(panel);
    const anchor = trigger.getBoundingClientRect();
    const mon = findDisplayCssAt(anchor.left, anchor.top);
    panel.style.maxHeight = `${Math.max(1, (mon.h - 16) / scale)}px`;
    panel.style.maxWidth = `${Math.max(1, (mon.w - 16) / scale)}px`;
    // 菜单和布局编辑器只跟随入口与屏幕边界，无须避让截图内容。
    const p = placeActionMenu(anchor, panel.offsetWidth * scale, panel.offsetHeight * scale, mon);
    panel.style.left = `${p.left}px`; panel.style.top = `${p.top}px`;
}
export function applyActionLayout(value) {
    layout = normalizeLayout(value);
    appliedLayoutKey = null;
    refreshActionLayout(widthBudget);
}
export function refreshActionLayout(availableWidth = Infinity) {
    if (!group || !buttons) return 1;
    widthBudget = availableWidth;
    const count = layout.toolbar.length;
    buttons.qr.disabled = ss.editorSession.source === 'long-screenshot';
    const qrTitle = buttons.qr.disabled ? t('ss.qr.unsupported') : t('ss.action.qr');
    if ((buttons.qr.getAttribute('data-tip') || buttons.qr.title) !== qrTitle) buttons.qr.title = qrTitle;
    const layoutKey = JSON.stringify([layout, Math.floor(availableWidth), ss.toolbar.classList.contains('toolbar-mode-gif-prep'),
        ...[...Object.values(buttons), document.getElementById('btn-ocr-diag')].map(button => [button.hidden, button.classList.contains('hidden')])]);
    if (layoutKey === appliedLayoutKey) {
        ss._reflowActionPanels?.();
        return Math.min(1, availableWidth / Math.max(1, ss.toolbar.offsetWidth));
    }
    appliedLayoutKey = layoutKey;
    let shown = fitLayout(layout, count);
    const place = result => {
        const divider = document.getElementById('ss-more-divider');
        for (const id of result.toolbar) { buttons[id].classList.remove('ss-menu-action'); buttons[id].removeAttribute('role'); group.insertBefore(buttons[id], divider || trigger); }
        for (const id of result.more) { buttons[id].classList.add('ss-menu-action'); buttons[id].setAttribute('role', 'menuitem'); }
        // Insert in reverse so the menu preserves the user's order before its fixed footer.
        for (const id of [...result.more].reverse()) menu.insertBefore(buttons[id], menu.firstChild);
        if (divider) divider.hidden = !result.toolbar.some(id => !buttons[id].hidden && !buttons[id].classList.contains('hidden'));
    };
    place(shown);
    // Measure the real toolbar, including left tools and fixed completion actions.
    for (let keep = count; keep > 0 && ss.toolbar.offsetWidth > availableWidth; keep--) {
        shown = fitLayout(layout, keep - 1); place(shown);
    }
    ss._reflowActionPanels?.();
    // Extremely narrow screens may be smaller than the left tools plus completion area.
    // Only then reduce the whole bar's visual scale; its order and fixed actions stay intact.
    return Math.min(1, availableWidth / Math.max(1, ss.toolbar.offsetWidth));
}
export function bindActionMenu() {
    group = document.querySelector('[data-group="output"]');
    trigger = document.getElementById('btn-more');
    menu = document.getElementById('ss-action-menu');
    buttons = Object.fromEntries(ACTION_IDS.map(id => [id, document.getElementById(`btn-${id}`)]));
    for (const id of ACTION_IDS) {
        const label = document.createElement('span'); label.className = 'ss-action-label'; label.textContent = t(`ss.action.${id}`);
        buttons[id].append(label); buttons[id].setAttribute('aria-label', label.textContent);
    }
    applyI18nFromConfig().then(() => {
        for (const id of ACTION_IDS) {
            buttons[id].querySelector('.ss-action-label').textContent = t(`ss.action.${id}`);
            buttons[id].setAttribute('aria-label', t(`ss.action.${id}`));
        }
        trigger.title = t('ss.more'); customize.textContent = t('ss.customize');
    }).catch(() => {});
    const customize = document.getElementById('ss-customize');
    let reflowFrame = 0;
    const reflow = () => {
        if (reflowFrame || (menu.hidden && !['screenshot-layout-editor', 'ocr-panel', 'qr-panel'].some(id => document.getElementById(id)))) return;
        reflowFrame = requestAnimationFrame(() => {
            reflowFrame = 0;
            if (!menu.hidden) positionActionPanel(menu);
            document.getElementById('screenshot-layout-editor')?._reposition();
            document.getElementById('ocr-panel')?._reposition();
            document.getElementById('qr-panel')?._reposition();
        });
    };
    // The toolbar owns DPI compensation; follow its final transform and monitor position.
    new MutationObserver(reflow).observe(ss.toolbar, {attributes: true, attributeFilter: ['style', 'data-ui-scale']});
    window.addEventListener('resize', reflow);
    ss._reflowActionPanels = reflow;
    const open = () => {
        if (closeActionMenu()) return;
        ss._closeToolDropdowns?.();
        menu.hidden = false; trigger.setAttribute('aria-expanded', 'true'); positionActionPanel(menu);
        menu.querySelector('button:not(:disabled):not([hidden]):not(.hidden)')?.focus();
    };
    trigger.onclick = e => { e.stopPropagation(); open(); };
    trigger.onkeydown = e => { if (e.key === 'ArrowDown') { e.preventDefault(); if (menu.hidden) open(); } };
    menu.addEventListener('mousedown', e => e.stopPropagation());
    menu.addEventListener('click', e => {
        e.stopPropagation();
        const button = e.target.closest('.ss-menu-action');
        if (button && !button.disabled) closeActionMenu();
    });
    menu.addEventListener('keydown', e => {
        // 未处理的按键也不冒泡到画布（方向键、标注和退出各有自己的语义）。
        e.stopPropagation();
        if (e.isComposing || e.keyCode === 229) return;
        if (!['ArrowDown', 'ArrowUp', 'Home', 'End', 'Escape', 'Tab'].includes(e.key)) return;
        e.preventDefault();
        if (e.key === 'Escape' || e.key === 'Tab') { closeActionMenu({focus: true}); return; }
        const items = [...menu.querySelectorAll('button')].filter(b => !b.disabled && !b.hidden && !b.classList.contains('hidden'));
        const index = items.indexOf(document.activeElement);
        const next = e.key === 'Home' ? 0 : e.key === 'End' ? items.length - 1 : (index + (e.key === 'ArrowUp' ? -1 : 1) + items.length) % items.length;
        items[next]?.focus();
    });
    customize.onclick = () => {
        closeActionMenu();
        openLayoutEditor({position: positionActionPanel, returnFocus: trigger, onSaved: () => ss._reloadScreenshotConfig?.()});
    };
    document.addEventListener('mousedown', e => { if (!menu.contains(e.target) && e.target !== trigger && !trigger.contains(e.target)) closeActionMenu(); });
    listen(EVENTS.CONFIG_CHANGED, event => {
        if (event.payload?.key === 'screenshot:config') ss._reloadScreenshotConfig?.();
    }).catch(() => {});
    ss._actionMenuVisible = () => !menu.hidden;
    ss._closeActionMenu = closeActionMenu;
    ss._closeLayoutEditor = () => closeLayoutEditor({force: true});
    ss._refreshActionLayout = refreshActionLayout;
    refreshActionLayout();
}
