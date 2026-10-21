//! 0.25.5：设置页/欢迎页共用全局键校验与注册状态语义。
//! 0.26.18：主键白名单扩展（OEM 标点/方向键/导航键），拒绝原因结构化返回
//! 供录制 UI 组装带具体原因的常驻报错。

const MODIFIER_ALIASES = {
    lctrl: "ctrl", rctrl: "ctrl", control: "ctrl",
    lalt: "alt", ralt: "alt",
    lshift: "shift", rshift: "shift",
    win: "meta", super: "meta",
};
const MODIFIER_ORDER = ["ctrl", "alt", "shift", "meta"];

/** OEM 标点主键（VK_OEM_1..8 的 US 布局未加 Shift 字符，录制产出形态）。 */
const OEM_PUNCTUATION = "`~-=[]\\;',./";
/** 方向键/导航键主键（与后端 vk_to_key 键名逐字对齐）。 */
const NAMED_INPUT_KEYS = ["up", "down", "left", "right", "pageup", "pagedown", "home", "end"];

function isFunctionKey(key) {
    return /^f([1-9]|1[01])$/.test(key);
}

function isInputKey(key) {
    return /^[a-z0-9]$/.test(key) || key === " " || OEM_PUNCTUATION.includes(key) || NAMED_INPUT_KEYS.includes(key);
}

/** 修饰键数组 → canonical 名数组；含未知修饰键时返回 null。 */
function canonicalModifiers(modifiers) {
    const mapped = modifiers.map((m) => {
        const lower = String(m).toLowerCase();
        return MODIFIER_ALIASES[lower] || lower;
    });
    return mapped.some((m) => !MODIFIER_ORDER.includes(m)) ? null : mapped;
}

/**
 * 解释一次录制为何被全局键白名单拒绝；有效组合返回 null。
 *
 * 0.26.18 起录制 UI 据此组装带具体原因的常驻报错（哪个键/修饰键不合规）。
 * code：malformed（输入结构非法）/ unsupported_key / unsupported_modifier /
 * modifier_required（无 Ctrl/Alt/Win——裸键非 F1–F11 或纯 Shift 组合）。
 * 与 normalizeRecordedGlobalHotkey 同源：两者对同一输入的判定不可分叉。
 */
export function explainGlobalHotkeyRejection(record) {
    if (!Array.isArray(record?.modifiers) || typeof record?.key !== "string" || record.key === "") {
        return {code: "malformed"};
    }
    const mods = canonicalModifiers(record.modifiers);
    if (!mods) {
        const unknown = record.modifiers
            .map((m) => {
                const lower = String(m).toLowerCase();
                return MODIFIER_ALIASES[lower] || lower;
            })
            .find((m) => !MODIFIER_ORDER.includes(m));
        return {code: "unsupported_modifier", modifier: unknown};
    }
    const key = record.key.toLowerCase();
    if (!isFunctionKey(key) && !isInputKey(key) && key !== "space") {
        return {code: "unsupported_key", key: record.key};
    }
    if (mods.length === 0) {
        return isFunctionKey(key) ? null : {code: "modifier_required"};
    }
    if (!mods.some((m) => ["ctrl", "alt", "meta"].includes(m))) {
        return {code: "modifier_required"};
    }
    return null;
}

/** 返回可保存的 canonical 组合；不支持的按键返回 null（原因见 explainGlobalHotkeyRejection）。 */
export function normalizeRecordedGlobalHotkey(record) {
    if (explainGlobalHotkeyRejection(record)) return null;
    const mods = canonicalModifiers(record.modifiers);
    const key = record.key.toLowerCase();
    return {modifiers: MODIFIER_ORDER.filter((m) => mods.includes(m)), key: key === "space" ? " " : key};
}

export function globalHotkeyStatusTextKey(status) {
    if (!status) return "chord.global.status.pending";
    if (status.registered) return "chord.global.status.active";
    if (status.reason === "occupied") return "chord.global.status.occupied";
    if (status.reason === "invalid") return "chord.global.status.invalid";
    return "chord.global.status.error";
}

export function canRetryGlobalHotkey(status) {
    return !!status && !status.registered && ["occupied", "error"].includes(status.reason);
}

/** 欢迎页开关不覆盖已配置的 Custom；Chord 键与全局键分别展示。 */
export function configuredGlobalHotkey(binding, defaultKey) {
    if (!binding?.global) return null;
    return binding.global.mode === "custom"
        ? {modifiers: binding.global.modifiers || [], key: binding.global.key || ""}
        : {modifiers: binding.modifiers || ["alt"], key: binding.key || defaultKey};
}
