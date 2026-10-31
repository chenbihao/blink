// 展开状态属于本窗口交互状态；后台刷新配置时按插件身份恢复，不保存输入内容。
export function capturePluginDisclosure(container) {
    const snapshot = new Map();
    for (const card of container.querySelectorAll('.extension-card[data-plugin-id]')) {
        snapshot.set(card.dataset.pluginId, {
            open: card.querySelector('.plugin-config-section')?.open === true,
            groups: new Map([...card.querySelectorAll('.plugin-group')]
                .map(group => [group.dataset.group, group.open === true])),
        });
    }
    return snapshot;
}

export function restorePluginDisclosure(container, snapshot) {
    for (const card of container.querySelectorAll('.extension-card[data-plugin-id]')) {
        const saved = snapshot.get(card.dataset.pluginId);
        if (!saved) continue;
        const section = card.querySelector('.plugin-config-section');
        if (section) section.open = saved.open;
        for (const group of card.querySelectorAll('.plugin-group')) {
            group.open = saved.groups.get(group.dataset.group) === true;
        }
    }
}
