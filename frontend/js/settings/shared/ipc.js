/**
 * IPC 封装模块
 * 封装 Tauri invoke 调用，提供统一的错误处理
 */

import { invoke } from "../../tauri.js";
import { setCurrentConfig } from "./state.js";

/**
 * 加载配置并更新共享状态
 * @returns {Promise<Object>} 配置对象
 */
export async function loadConfig() {
  const cfg = await invoke("get_config");
  setCurrentConfig(cfg);
  return cfg;
}

/**
 * 保存配置到后端
 * @param {string} key - 配置键
 * @param {*} value - 配置值
 */
export async function saveConfigToBackend(key, value) {
  await invoke("set_config", { key, value });
}

/**
 * 获取引擎配置
 * @param {string} engineId - 引擎 ID
 * @returns {Promise<Object>} 引擎配置
 */
export async function getEngineConfig(engineId) {
  return await invoke("get_engine_config", { engineId });
}

/**
 * 获取上下文配置
 * @returns {Promise<Object>} 上下文配置
 */
export async function getContextConfig() {
  return await invoke("get_context_config");
}

/**
 * 获取开始菜单配置
 * @returns {Promise<Object>} 开始菜单配置
 */
export async function getStartMenuConfig() {
  return await invoke("get_start_menu_config");
}

/**
 * 获取计算器配置
 * @returns {Promise<Object>} 计算器配置
 */
export async function getCalcConfig() {
  return await invoke("get_calc_config");
}

/**
 * 列出内置动作
 * @returns {Promise<Array>} 内置动作列表
 */
export async function listBuiltinActions() {
  return await invoke("list_builtin_actions");
}

/**
 * 列出上下文绑定
 * @returns {Promise<Array>} 上下文绑定列表
 */
export async function listContextBindings() {
  return await invoke("list_context_bindings");
}

/**
 * 隐藏设置窗口
 */
export async function hideSettingsWindow() {
  await invoke("hide_settings_window");
}
