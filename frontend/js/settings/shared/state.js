/**
 * 共享状态模块
 * 持有跨 Tab 共享的当前配置（从后端加载）。
 */

/** @type {Object|null} 当前配置（从后端加载） */
export let currentConfig = null;

/** 设置当前配置 */
export function setCurrentConfig(config) {
    currentConfig = config;
}

/** 获取当前配置 */
export function getCurrentConfig() {
    return currentConfig;
}
