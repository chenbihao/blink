//! 剪贴板条目身份与操作描述。选择集不依赖默认 action、路径字段或内容载荷。
export function normalizeRef(ref) {
    return ref && typeof ref.kind === 'string' && ref.kind.length > 0
        && typeof ref.id === 'string' && ref.id.length > 0
        ? {kind: ref.kind, id: ref.id} : null;
}
export function entryRef(item) { return normalizeRef(item?.clipboard_ref ?? item?.clipboardRef); }
export function refKey(ref) { return JSON.stringify([ref.kind, ref.id]); }
export function refFromKey(key) {
    try { const [kind, id] = JSON.parse(key); return normalizeRef({kind, id}); }
    catch { return null; }
}
export function refFromLi(li) {
    return normalizeRef({kind: li?.dataset.clipboardKind, id: li?.dataset.clipboardId});
}
export function refsOfKind(refs, kind) { return refs.filter(ref => ref.kind === kind); }

// 类型声明顺序也是菜单的固定类型顺序；未知类型不会落入文本执行路径。
const TYPE_OPERATIONS = Object.assign(Object.create(null), {
    text: ['copy', 'paste', 'edit', 'sticky', 'delete'],
    image: ['pin', 'delete'],
});
export function selectionActions(refs) {
    const actions = [];
    for (const id of ['copy', 'paste', 'edit', 'sticky', 'pin']) {
        const targets = refs.filter(ref => TYPE_OPERATIONS[ref.kind]?.includes(id));
        if (targets.length) actions.push({id, refs: targets, count: targets.length,
            shortcut: targets.length === refs.length
                ? ({copy: 'Ctrl+C', paste: 'Enter', edit: 'Alt+E', sticky: 'Alt+S'}[id] || '') : ''});
    }
    const deletable = refs.filter(ref => TYPE_OPERATIONS[ref.kind]?.includes('delete'));
    if (deletable.length) {
        const selectedKinds = new Set(deletable.map(ref => ref.kind));
        const kinds = Object.keys(TYPE_OPERATIONS).filter(kind => selectedKinds.has(kind));
        if (kinds.length > 1) for (const kind of kinds) {
            const targets = refsOfKind(deletable, kind);
            actions.push({id: 'delete', refs: targets, count: targets.length, scope: kind, danger: true});
        }
        actions.push({id: 'delete', refs: deletable, count: deletable.length,
            scope: deletable.length === refs.length ? 'all' : 'supported', danger: true});
    }
    return actions;
}
export function supportsTextOperation(refs) {
    return refs.length > 0 && refs.every(ref => ref.kind === 'text');
}
