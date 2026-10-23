/**
 * composer 弹层互斥仲裁（0.27.7，phase §九）。
 *
 * 同一 composer 输入区的弹层——`/` 命令面板、模型下拉、思考下拉、提示词选择
 * （后续 @ 附件面板接入同一入口）——同刻最多开一个：打开任一弹层前先关闭
 * 其余全部，不允许各自为政的关闭逻辑。
 *
 * 各模块保留自己弹层的 DOM 控制，只把 close 函数注册进来；`openExclusive(id)`
 * 负责关掉其他弹层后返回，由调用方随后打开自身（保持注册/打开顺序简单：
 * 先 openExclusive，再 hidden=false） */

/** @type {Map<string, () => void>} id → close（幂等，重复注册覆盖旧值） */
const popups = new Map();

/**
 * 注册弹层。close 需幂等（已关闭时再调无副作用）。
 * @param {string} id 弹层标识（"slash" / "model" / "thinking" ...）
 * @param {() => void} close
 */
export function registerPopup(id, close) {
    popups.set(id, close);
}

/**
 * 打开弹层前的互斥仲裁：关闭除 `id` 外的全部弹层。
 * 不负责打开 `id` 自身——由调用方随后执行，避免本模块了解各弹层打开细节。
 * @param {string} id 即将打开的弹层 id
 */
export function openExclusive(id) {
    for (const [pid, close] of popups) {
        if (pid !== id) close();
    }
}
