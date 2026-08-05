//! 窗口高度自适配：测 #app 真实渲染高度，回传 resize_window。
//!
//! 取代旧的硬编码估算（INPUT_HEIGHT + itemCount*ITEM_HEIGHT）。CSS 的 padding/
//! 圆角/间距随便调，窗口都自动贴合内容——改样式不必再回头改 JS。
//!
//! **maxHeight 踩踏**（翻页不缩窗口）：同一搜索会话内，窗口高度只增不减。
//! 翻到末页（不足 PAGE_SIZE 条）时窗口不缩小，避免鼠标滚轮溢出到背后窗口。
//! 新搜索（seq 变化 / clear）时调 `resetMaxHeight()` 归零，让窗口正确收缩。

import { appEl } from "./dom.js";
import { resizeWindow } from "../shared/api.js";

/** 窗口宽度（暂固定，未来可配置）。 */
const WIDTH = 700;

/** 当前搜索会话内的最大高度（px）。0 表示未初始化。 */
let maxHeight = 0;

/**
 * 在下一帧（layout 完成后）测量 #app 实际高度并 resize 窗口。
 * 必须等 rAF：DOM 刚改完直接读高度会拿到旧 layout。
 *
 * maxHeight 机制：同一搜索会话内窗口只增不减。
 * - 当前内容更高 → 更新 maxHeight，窗口长高
 * - 当前内容更矮（末页不足一页）→ 保持 maxHeight，窗口不缩
 * - maxHeight 归零时（新搜索 / clear）→ 按实际内容高度设置
 */
export function syncWindowSize() {
  requestAnimationFrame(() => {
    // offsetHeight 含 padding/border，正是窗口需要的物理内容高度
    const height = appEl.offsetHeight;
    if (height > maxHeight) {
      maxHeight = height;
    }
    // 用 minHeight 让 #app 撑满窗口高度（避免 #app 下方出现透明间隙）
    appEl.style.minHeight = maxHeight + "px";
    resizeWindow(WIDTH, maxHeight);
  });
}

/**
 * 归零 maxHeight（新搜索会话开始时调）。
 *
 * 清空 minHeight 内联样式，让 #app 回归内容驱动高度，
 * 随后的 syncWindowSize() 会按实际内容重新踩踏。
 */
export function resetMaxHeight() {
  maxHeight = 0;
  appEl.style.minHeight = "";
}
