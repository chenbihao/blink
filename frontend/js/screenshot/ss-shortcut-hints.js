/** 工具组随 Alt、操作组随 Ctrl 显示；失焦/会话切换清理残留。 */
export function bindShortcutHints(target, body) {
    const clear = () => {
        delete body.dataset.altDown;
        delete body.dataset.ctrlDown;
    };
    const update = (event, down) => {
        const field = event.key === 'Alt' ? 'altDown' : event.key === 'Control' ? 'ctrlDown' : null;
        if (!field) return;
        // 抑制 WebView2 Alt 菜单激活，避免吞 keyup；Ctrl 保留原生输入行为。
        if (event.key === 'Alt') event.preventDefault();
        if (down) body.dataset[field] = 'true';
        else delete body.dataset[field];
    };
    target.addEventListener('keydown', event => update(event, true));
    target.addEventListener('keyup', event => update(event, false));
    target.addEventListener('blur', clear);
    return clear;
}
