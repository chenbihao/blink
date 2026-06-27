//! 结果项激活动作：根据 action.kind 决定行为（打开应用 / 复制结果）。
//!
//! 抽成独立模块，因为「激活」是 click 和键盘 Enter/Alt+数字的共同终点，
//! 且未来动作类型会增多（插件动作、文件打开等）——集中在此便于扩展。
//! 行为由后端提供的 action.kind 驱动，与提示栏（hints.js）同源，语义一致。

import { launchApp, hideWindow } from "./api.js";

/**
 * 激活一个结果项。
 * @param {{lnkPath?: string, calcValue?: string, payload?: string, action?: {kind: string}, isError?: boolean}} data
 */
export async function activateItem(data) {
  if (!data) return;
  // 错误信息项不可执行
  if (data.isError) return;
  const kind = data.action?.kind;

  if (kind === "copy") {
    // 复制结果到剪贴板；成功才隐藏。失败/空文本不隐藏，避免用户误以为已复制。
    // 插件 Copy 走 action.payload；计算结果无 payload 时回退 calcValue。
    const text = data.payload ?? data.calcValue;
    if (text) {
      try {
        await navigator.clipboard.writeText(text);
      } catch (e) {
        console.error("clipboard write failed:", e);
        return; // 保留窗口，让用户察觉并重试
      }
      hideWindow();
    }
    // 无文本(空串/缺省):不复制也不隐藏,让用户察觉该结果无可复制内容
    return;
  }

  // 默认 / open：启动应用（launch_app 内部会隐藏窗口）
  if (data.lnkPath) {
    try {
      await launchApp(data.lnkPath);
    } catch (e) {
      console.error("launch_app failed:", e);
    }
  }
}
