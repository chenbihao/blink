//! 剪贴板文本多选纯状态模块（0.20.2）。
//!
//! **设计动机**（§5.3）：
//! 剪贴板模式下的多选需要跨页保持稳定。翻页或同 query 增量重排后，
//! 选择状态应保留仍存在的 key，清理已消失的 key。
//! 此模块是纯逻辑（无 DOM 依赖），可单测跨页顺序、重排和删除。
//!
//! **状态契约**：
//! ```text
//! selectedKeys: Set<stableTextHitId>
//! selectionEpoch: number
//! copyGeneration: number
//! ```
//!
//! **键盘/鼠标规则**：
//! 1. IME composition 优先，期间不接管快捷键。
//! 2. `单击` 切换当前文本项选中态；图片项无选择态。
//! 3. query 为空时 `Ctrl+A` 全选当前返回的全部文本项；
//!    query 非空时保留输入框全选。
//! 4. 存在多选时 `Ctrl+C` 批量复制；不存在多选时保留输入框原生复制。
//! 5. `Esc` 顺序：清空多选 → 退出 clipboard mode → 隐藏窗口。

// ── 状态 ─────────────────────────────────────────────────────────────────────

/** 已选中的稳定文本 hitId 集合。 */
let selectedKeys = new Set();

/** 选择 epoch：每次进入剪贴板模式 +1，退出时 +1 使旧 epoch 失效。
 *  批量复制请求发起时记录当前 epoch，完成后比对 epoch 判断是否仍有效。 */
let selectionEpoch = 0;

/** 复制 generation：每次发起批量复制 +1，退出/隐藏/query 变化 +1。
 *  旧 generation 的请求结果不得写剪贴板。 */
let copyGeneration = 0;

// ── 状态查询 ─────────────────────────────────────────────────────────────────

/** 获取当前选中 key 列表（按插入顺序排列）。 */
export function getSelectedKeys() {
    return [...selectedKeys];
}

/** 当前是否有任何选中项。 */
export function hasSelection() {
    return selectedKeys.size > 0;
}

/** 选中项数量。 */
export function selectedCount() {
    return selectedKeys.size;
}

/** 获取当前 epoch（批量复制时快照用）。 */
export function getSelectionEpoch() {
    return selectionEpoch;
}

/** 获取当前 copy generation（批量复制发起时快照用）。 */
export function getCopyGeneration() {
    return copyGeneration;
}

/** 检查某个 key 是否已选中。 */
export function isSelected(key) {
    return selectedKeys.has(key);
}

// ── 状态变更 ─────────────────────────────────────────────────────────────────

/**
 * 进入剪贴板模式时调用：递增 epoch 使旧选择失效，清空状态。
 */
export function onEnterMode() {
    selectionEpoch++;
    clearSelection();
}

/**
 * 退出剪贴板模式时调用：递增 epoch + generation，清空状态。
 */
export function onExitMode() {
    selectionEpoch++;
    copyGeneration++;
    clearSelection();
}

/**
 * 窗口隐藏时调用：递增 generation，清空状态。
 */
export function onWindowHidden() {
    copyGeneration++;
    clearSelection();
}

/**
 * query 变化时调用：递增 generation，清空状态。
 * 选择状态依赖当前结果集，query 变化意味着结果集会变。
 */
export function onQueryChanged() {
    copyGeneration++;
    clearSelection();
}

/**
 * 清空所有选中状态（不清 epoch/generation）。
 */
export function clearSelection() {
    selectedKeys = new Set();
}

// ── 选择操作 ─────────────────────────────────────────────────────────────────

/**
 * 单击：切换当前项的选中态。
 * 图片项不进入选择集合（调用方负责过滤）。
 * @param {string} key 当前项的 stableTextHitId
 * @returns {boolean} 切换后该 key 是否选中
 */
export function toggleSelection(key) {
    if (!key) return false;
    if (selectedKeys.has(key)) {
        selectedKeys.delete(key);
        return false;
    } else {
        selectedKeys.add(key);
        return true;
    }
}

/**
 * Ctrl+A（query 为空时）：全选当前返回的全部文本项。
 * @param {string[]} allTextKeys 当前全局结果中所有文本项的 hitId
 */
export function selectAll(allTextKeys) {
    selectedKeys = new Set(allTextKeys);
}

/**
 * 翻页或同 query 增量重排后，保留仍存在的 key，清理已消失的 key。
 *
 * @param {string[]} currentTextKeys 重排后当前全局结果中所有文本项的 hitId
 */
export function reconcileAfterReorder(currentTextKeys) {
    const validSet = new Set(currentTextKeys);
    const newSelected = new Set();
    for (const key of selectedKeys) {
        if (validSet.has(key)) {
            newSelected.add(key);
        }
    }
    selectedKeys = newSelected;
}

/**
 * 发起批量复制时调用：递增 copy generation，返回新 generation。
 * 调用方持有此 generation，复制完成后比对判断是否仍有效。
 */
export function beginCopy() {
    copyGeneration++;
    return copyGeneration;
}

/**
 * 检查 generation 和 epoch 是否仍然有效（复制完成时调用）。
 * @param {number} gen beginCopy 返回的 generation
 * @returns {boolean} true = 仍然有效，可以写剪贴板
 */
export function isCopyStillValid(gen) {
    return gen === copyGeneration;
}

// ── 内部辅助（供单测使用）──────────────────────────────────────────────────

/** 重置所有状态（单测用）。 */
export function _resetForTest() {
    selectedKeys = new Set();
    selectionEpoch = 0;
    copyGeneration = 0;
}
