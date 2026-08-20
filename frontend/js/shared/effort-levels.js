/**
 * 思考强度预设档位（reasoning_effort 线值）——前端单一真源（0.21.23）。
 *
 * 此前 main.js / model-edit.js / settings.html 三份各自维护同一列表，
 * 存在漂移风险。现在 JS 侧统一从此处导入；settings.html 的静态 <option>
 * 由 model-edit.js 打开弹窗时按此表动态渲染（含 i18n 文案）。
 *
 * 档位语义与后端 `domain/ai/thinking.rs` 对齐：原文本直传供应商
 * （不翻译——中文译名对不上 xhigh 等供应商档位）。
 * 新增档位时同步此表。
 */
export const EFFORT_LEVELS = ["minimal", "low", "medium", "high", "xhigh", "max"];

/**
 * 判断 effort 线值是否为预设档位（不含 ""/none/custom）。
 * @param {string} effort
 * @returns {boolean}
 */
export function isPresetEffort(effort) {
    return EFFORT_LEVELS.includes(effort);
}
