//! 原生 select 保留为逻辑真源，菜单与按钮统一使用编辑器主题。
export function bindEditorSelects(documentRef = document) {
    const controls = new Map();
    let opened = null;
    function close(focus = false) {
        if (!opened) return;
        const current = opened; opened = null;
        if (current.menu.matches(':popover-open')) current.menu.hidePopover();
        current.trigger.setAttribute('aria-expanded', 'false');
        if (focus) current.trigger.focus({preventScroll:true});
    }
    function sync() {
        for (const [select, control] of controls) {
            const text = select.selectedOptions[0]?.textContent || '—';
            if (control.label.textContent !== text) control.label.textContent = text;
            control.trigger.disabled = select.disabled;
            const name = select.getAttribute('aria-label') || select.closest('label')?.querySelector('span')?.textContent || select.id;
            control.trigger.setAttribute('aria-label', `${name}: ${text}`);
            control.menu.setAttribute('aria-label', name);
            if (select.disabled && opened === control) close();
        }
    }
    function open(select, control) {
        if (opened === control) {close(true); return;}
        close(); sync();
        control.menu.replaceChildren();
        for (const option of select.options) {
            if (option.hidden) continue;
            const button = documentRef.createElement('button');
            button.type = 'button'; button.className = 'editor-select-option';
            button.textContent = option.textContent; button.disabled = option.disabled;
            button.setAttribute('role', 'option'); button.setAttribute('aria-selected', String(option.selected));
            button.addEventListener('click', () => {
                select.value = option.value;
                select.dispatchEvent(new Event('change', {bubbles:true}));
                close(true); sync();
            });
            control.menu.append(button);
        }
        const rect = control.trigger.getBoundingClientRect();
        control.menu.style.minWidth = `${Math.min(rect.width, window.innerWidth - 16)}px`;
        control.menu.style.maxWidth = `${window.innerWidth - 16}px`;
        control.menu.style.maxHeight = `${Math.max(40, window.innerHeight - 16)}px`;
        control.menu.showPopover();
        const width = control.menu.offsetWidth, height = control.menu.offsetHeight;
        const below = rect.bottom + 6, above = rect.top - height - 6;
        control.menu.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - width - 8))}px`;
        control.menu.style.top = `${Math.max(8, Math.min(below + height <= window.innerHeight - 8 ? below : above, window.innerHeight - height - 8))}px`;
        opened = control; control.trigger.setAttribute('aria-expanded', 'true');
        (control.menu.querySelector('[aria-selected="true"]:not(:disabled)') || control.menu.querySelector('button:not(:disabled)'))?.focus({preventScroll:true});
    }
    for (const select of documentRef.querySelectorAll('select')) {
        const trigger = documentRef.createElement('button'), label = documentRef.createElement('span');
        trigger.type = 'button'; trigger.className = 'editor-select-trigger'; trigger.append(label);
        // 同一图标 sprite；不引入另一套图标资源或前端构建。
        trigger.insertAdjacentHTML('beforeend', '<svg class="icon" aria-hidden="true"><use href="#icon-chevron-down"/></svg>');
        const menu = documentRef.createElement('div');
        menu.id = `${select.id}-menu`; menu.className = 'editor-select-menu';
        menu.setAttribute('popover', 'auto'); menu.setAttribute('role', 'listbox');
        trigger.setAttribute('aria-haspopup', 'listbox'); trigger.setAttribute('aria-expanded', 'false');
        trigger.setAttribute('aria-controls', menu.id);
        select.after(trigger);
        // 导出菜单必须留在 dialog 的 top layer 子树内。
        (select.closest('dialog') || documentRef.body).append(menu);
        select.classList.add('editor-native-select'); select.setAttribute('aria-hidden', 'true'); select.tabIndex = -1;
        const control = {trigger, label, menu}; controls.set(select, control);
        trigger.addEventListener('click', () => open(select, control));
        trigger.addEventListener('keydown', event => {
            if (['ArrowDown', 'ArrowUp'].includes(event.key)) {event.preventDefault(); open(select, control);}
        });
        menu.addEventListener('keydown', event => {
            if (event.key === 'Escape') {event.preventDefault(); event.stopPropagation(); close(true); return;}
            if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
            event.preventDefault();
            const items = [...menu.querySelectorAll('button:not(:disabled)')], index = items.indexOf(documentRef.activeElement);
            const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 :
                (index + (event.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length;
            items[next]?.focus({preventScroll:true});
        });
        menu.addEventListener('toggle', event => {
            if (event.newState === 'closed') {
                trigger.setAttribute('aria-expanded', 'false');
                if (opened === control) opened = null;
            }
        });
        select.addEventListener('change', () => queueMicrotask(sync));
        new MutationObserver(() => { if (opened === control) close(); sync(); })
            .observe(select, {attributes:true, childList:true, characterData:true, subtree:true});
    }
    window.addEventListener('resize', () => close());
    documentRef.addEventListener('scroll', event => {
        // 播放头会自动横滚时间轴，参数菜单的锚点没有移动。
        if (event.target.id === 'timeline-scroll') return;
        if (opened?.menu.contains(event.target)) return;
        close();
    }, true);
    sync();
    return sync;
}
