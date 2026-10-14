//! 来源无关的剪贴板选择状态：稳定 key、区间锚点和异步快照版本。
let selectedKeys = new Set();
let anchorKey = null;
let rangeBase = null;
let rangeAdditive = false;
let selectionEpoch = 0;
let selectionRevision = 0;
let copyGeneration = 0;

export function getSelectedKeys() { return [...selectedKeys]; }
export function hasSelection() { return selectedKeys.size > 0; }
export function selectedCount() { return selectedKeys.size; }
export function getSelectionEpoch() { return selectionEpoch; }
export function getSelectionRevision() { return selectionRevision; }
export function getCopyGeneration() { return copyGeneration; }
export function getAnchorKey() { return anchorKey; }
export function isSelected(key) { return selectedKeys.has(key); }

export function clearSelection() {
    selectedKeys = new Set();
    anchorKey = null;
    rangeBase = null;
    selectionRevision++;
}
export function onEnterMode() { selectionEpoch++; copyGeneration++; clearSelection(); }
export function onExitMode() { selectionEpoch++; copyGeneration++; clearSelection(); }
export function onWindowHidden() { copyGeneration++; clearSelection(); }
export function onQueryChanged() { selectionEpoch++; copyGeneration++; clearSelection(); }

export function selectSingle(key) {
    if (!key) return;
    selectedKeys = new Set([key]);
    anchorKey = key;
    rangeBase = null;
    selectionRevision++;
}
export function toggleSelection(key) {
    if (!key) return false;
    if (selectedKeys.has(key)) selectedKeys.delete(key);
    else selectedKeys.add(key);
    anchorKey = key;
    rangeBase = null;
    selectionRevision++;
    return selectedKeys.has(key);
}
export function selectAll(keys) {
    selectedKeys = new Set(keys);
    anchorKey = keys[0] ?? null;
    rangeBase = null;
    selectionRevision++;
}
export function selectRange(orderedKeys, target, additive = false) {
    const end = orderedKeys.indexOf(target);
    if (end < 0) return;
    let start = orderedKeys.indexOf(anchorKey);
    if (start < 0) { anchorKey = target; start = end; }
    if (rangeBase === null || rangeAdditive !== additive) {
        rangeBase = additive ? new Set(selectedKeys) : new Set();
        rangeAdditive = additive;
    }
    selectedKeys = new Set([...rangeBase, ...orderedKeys.slice(Math.min(start, end), Math.max(start, end) + 1)]);
    selectionRevision++;
}
export function removeKeys(keys) {
    for (const key of keys) selectedKeys.delete(key);
    if (keys.includes(anchorKey)) anchorKey = null;
    rangeBase = null;
    selectionRevision++;
}
export function reconcileAfterReorder(keys) {
    const valid = new Set(keys);
    const removed = [...selectedKeys].filter(key => !valid.has(key));
    if (removed.length) removeKeys(removed);
    if (!valid.has(anchorKey)) { anchorKey = null; rangeBase = null; }
}
export function capture() {
    return {keys: getSelectedKeys(), epoch: selectionEpoch, revision: selectionRevision};
}
export function isCurrent(snapshot) { return snapshot.epoch === selectionEpoch; }
/** 不清理新选择手势，成功只清快照目标，其他类型保留。 */
export function complete(snapshot, keys = snapshot.keys) {
    if (isCurrent(snapshot) && snapshot.revision === selectionRevision) removeKeys(keys);
}
export function beginCopy() { return ++copyGeneration; }
export function isCopyStillValid(gen) { return gen === copyGeneration; }
export function _resetForTest() {
    selectedKeys = new Set(); anchorKey = null; rangeBase = null;
    selectionEpoch = 0; selectionRevision = 0; copyGeneration = 0;
}
