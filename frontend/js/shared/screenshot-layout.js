// Stable ids are shared with ScreenshotToolbarLayout in the domain config.
export const DEFAULT_LAYOUT = Object.freeze({
    toolbar: Object.freeze(['translate', 'scroll', 'gif', 'save', 'ask-ai', 'pin']),
    more: Object.freeze(['ocr', 'qr', 'translate-pin']),
});
export const ACTION_IDS = Object.freeze([...DEFAULT_LAYOUT.toolbar, ...DEFAULT_LAYOUT.more]);

export function normalizeLayout(value) {
    if (!value || typeof value !== 'object') return structuredClone(DEFAULT_LAYOUT);
    const seen = new Set();
    const clean = list => (Array.isArray(list) ? list : []).filter(id => {
        if (!ACTION_IDS.includes(id) || seen.has(id)) return false;
        seen.add(id);
        return true;
    });
    const toolbar = clean(value.toolbar);
    const more = clean(value.more);
    more.push(...ACTION_IDS.filter(id => !seen.has(id)));
    return {toolbar, more};
}

export function fitLayout(value, count) {
    const layout = normalizeLayout(value);
    const keep = Math.max(0, Math.min(layout.toolbar.length, count));
    return {toolbar: layout.toolbar.slice(0, keep), more: [...layout.toolbar.slice(keep), ...layout.more]};
}

export function moveLayoutAction(value, id, zone, index) {
    const layout = normalizeLayout(value);
    if (!ACTION_IDS.includes(id) || !['toolbar', 'more'].includes(zone)) return layout;
    for (const key of ['toolbar', 'more']) layout[key] = layout[key].filter(item => item !== id);
    layout[zone].splice(Math.max(0, Math.min(index, layout[zone].length)), 0, id);
    return layout;
}

export function dropLayoutAction(value, id, zone, targetId, after = false) {
    const layout = normalizeLayout(value);
    if (!['toolbar', 'more'].includes(zone)) return layout;
    const targetIndex = layout[zone].indexOf(targetId);
    let index = targetIndex < 0 ? layout[zone].length : targetIndex + Number(after);
    const sourceIndex = layout[zone].indexOf(id);
    if (sourceIndex >= 0 && sourceIndex < index) index--;
    return moveLayoutAction(layout, id, zone, index);
}
