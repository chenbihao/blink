//! 0.25.5：设置页/欢迎页共用全局键校验与注册状态语义。

const MODIFIER_ALIASES = {
    lctrl: "ctrl", rctrl: "ctrl", control: "ctrl",
    lalt: "alt", ralt: "alt",
    lshift: "shift", rshift: "shift",
    win: "meta", super: "meta",
};
const MODIFIER_ORDER = ["ctrl", "alt", "shift", "meta"];

/** 返回可保存的 canonical 组合；不支持的按键返回 null。 */
export function normalizeRecordedGlobalHotkey(record) {
    if (!Array.isArray(record?.modifiers) || typeof record?.key !== "string") return null;
    const mods = record.modifiers.map((m) => {
        const lower = String(m).toLowerCase();
        return MODIFIER_ALIASES[lower] || lower;
    });
    if (mods.some((m) => !MODIFIER_ORDER.includes(m))) return null;
    const key = record.key.toLowerCase();
    const functionKey = /^f([1-9]|1[01])$/.test(key);
    const inputKey = /^[a-z0-9]$/.test(key) || key === " " || key === "space";
    const usableModifier = mods.some((m) => ["ctrl", "alt", "meta"].includes(m));
    if (!(mods.length === 0 ? functionKey : usableModifier && (functionKey || inputKey))) return null;
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
