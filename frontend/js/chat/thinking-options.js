/**
 * 思考下拉选项构建（0.27.15）——纯函数，无 DOM 依赖。
 *
 * 能力快照字段是后端 `ThinkingCapability` 的 serde 投影（snake_case：
 * `can_disable` / `can_enable_default` / `default_effort`，与 status 载荷
 * 其余字段一致）——本模块是字段契约的回归锚点（thinking-options.test.mjs）。
 */

import {EFFORT_LEVELS} from "../shared/effort-levels.js";

/**
 * 按能力快照构建下拉选项描述。
 * @param {{shape?: string, levels?: string[], can_disable?: boolean,
 *          can_enable_default?: boolean, default_effort?: string|null}|null} cap
 * @param {string|null} current 当前选中线值（null/"" = 默认档）
 * @returns {{rows: Array<{type: "group", title: string}|{type: "option", effort: string, title: string, sub: string|null, selected: boolean}>,
 *            showCustomInput: boolean}}
 */
export function thinkingDropdownOptions(cap, current) {
    const cur = current ?? "";
    /** @type {Array<{type: "group", title: string}|{type: "option", effort: string, title: string, sub: string|null, selected: boolean}>} */
    const rows = [];
    const group = (title) => rows.push({type: "group", title});
    const option = (effort, title, sub, selected) =>
        rows.push({type: "option", effort, title, sub, selected: Boolean(selected)});

    if (!cap || cap.shape === "unknown") {
        // 能力未知：现状全量下拉（默认/关/预设档/自定义）+ 提示。
        // "default_on" 对下拉型模型与「默认」同为 omit（后端 EnableDefault→None），
        // 选中态归并到「默认」，避免按钮显示「开」而下拉无高亮。
        group("思考");
        option("", "默认", "不主动给模型打 patch", cur === "" || cur === "default_on");
        option("none", "思考关", null, cur === "none");
        group("思考级别");
        for (const level of EFFORT_LEVELS) {
            option(level, level, null, cur === level);
        }
        return {rows, showCustomInput: true};
    }

    // 能力已知（levels）：目录声明档位 + 默认/关/开·默认
    group("思考");
    option("", "默认", "跟随服务默认（不发送思考参数）", cur === "");
    if (cap.can_disable) {
        option("none", "关", null, cur === "none");
    }
    if (cap.can_enable_default) {
        option("default_on", "开", "默认强度 / 默认预算", cur === "default_on");
    }
    if (cap.levels && cap.levels.length > 0) {
        group("思考级别");
        for (const level of cap.levels) {
            const isDefault = cap.default_effort === level;
            option(level, level, isDefault ? "目录默认档" : null, cur === level);
        }
    }
    return {rows, showCustomInput: false};
}
