//! 窗口高度自适配：测 #app 真实渲染高度，回传 resize_window。
//!
//! 取代旧的硬编码估算（INPUT_HEIGHT + itemCount*ITEM_HEIGHT）。CSS 的 padding/
//! 圆角/间距随便调，窗口都自动贴合内容——改样式不必再回头改 JS。

import { appEl } from "./dom.js";
import { resizeWindow } from "./api.js";

/** 窗口宽度（暂固定，未来可配置）。 */
const WIDTH = 700;

/**
 * 在下一帧（layout 完成后）测量 #app 实际高度并 resize 窗口。
 * 必须等 rAF：DOM 刚改完直接读高度会拿到旧 layout。
 */
export function syncWindowSize() {
  requestAnimationFrame(() => {
    // offsetHeight 含 padding/border，正是窗口需要的物理内容高度
    const height = appEl.offsetHeight;
    resizeWindow(WIDTH, height);
  });
}
