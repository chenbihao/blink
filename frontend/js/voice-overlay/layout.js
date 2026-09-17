/**
 * voice-overlay 窗口尺寸决策（0.23.7）——纯函数，无 DOM / IPC 依赖，可测。
 *
 * 设计约束（spec-frontend §5.2 窗体尺寸稳定 + 0.23.7 任务范围三）：
 * - g2 hold 模式沿用既有"内容高度撑高"策略（MIN 140 / MAX 400，宽 260）；
 * - editor 连续听写模式使用**固定稳定尺寸**：预览文本在内部滚动，窗口不随
 *   识别文本持续跳动；宽度加宽到 304，让"返回编辑器"与另两个控制按钮
 *   都有足够空间；进入/收起（mini）各有一档固定高度。
 *
 * 宽高均为逻辑像素（CSS px），后端 `resize_voice_overlay` 以 LogicalSize 应用，
 * DPI 缩放由 Tauri 换算。
 */

/** g2 hold：按内容高度撑高（0.10.6 行为保持不变） */
export const G2_SIZE = {width: 260, minHeight: 140, maxHeight: 400};

/** editor 模式：稳定尺寸（约 300-320 宽 / 300-340 高区间的实测取值）。
 *  高度预算（0.23.7；0.23.13 预览区提到 8 行）：body padding 20 +
 *  overlay padding 22 + 3 个 gap 24 + 拖拽条 33 + 波形 20 + 控制条 30 = 149，
 *  预览区 max-height 157（8 行 × 14px × 1.4）→ 306，留 15px 余量，
 *  保证控制条不被窗口底边裁切。 */
export const EDITOR_SIZE = {width: 304, height: 321};

/** editor mini：只留拖拽条 + 波形（20 + 22 + 8 + 33 + 20 = 103，留 1px 余量） */
export const EDITOR_MINI_SIZE = {width: 304, height: 104};

/**
 * 解析当前应应用到窗口的逻辑尺寸。
 * @param {{mode: "g2"|"editor", mini?: boolean, contentHeight?: number}} state
 *   contentHeight 为 g2 模式下 document.body.scrollHeight（rAF 后测量值）。
 * @returns {{width: number, height: number}}
 */
export function resolveOverlaySize({mode, mini = false, contentHeight = 0}) {
    if (mode === "editor") {
        return mini ? {...EDITOR_MINI_SIZE} : {...EDITOR_SIZE};
    }
    const clamped = Math.min(
        Math.max(Number(contentHeight) || 0, G2_SIZE.minHeight),
        G2_SIZE.maxHeight,
    );
    return {width: G2_SIZE.width, height: clamped};
}
