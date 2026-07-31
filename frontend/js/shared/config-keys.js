/**
 * 泛型配置写入入口（0.8.6 P1-C 前端泛型化）。
 *
 * 前端统一调用 `saveConfig(key, value)` → 后端 `set_config` 命令按 key 路由到
 * 对应分片持久化 + 副作用（SearchService 热更新 / 平台 API / emit 事件）。
 *
 * # 支持的 key
 *
 * **AppConfig 分片**：`language` / `log_level` / `auto_start` / `hotkey` /
 * `tap_threshold` / `grace_period` / `general_config` / `autosuggest` /
 * `chord_toggles` / `chord_bindings`（0.10.7）/ `clipboard_enabled` /
 * `clipboard_config`（0.10.7）/ `disabled_builtin_actions` /
 * `disabled_context_bindings` / `disabled_chord_actions`
 *
 * **引擎配置**：`file_search` / `start_menu_config` / `calc_config` / `global_proxy`
 *
 * **插件配置**：`plugin_config`
 *
 * **Context 配置**：`context_config`
 *
 * **截图配置**（0.11.10-b）：`screenshot_config` —— ScreenshotConfig 分片（prewarm_ocr 等）
 *
 * **AI 配置**（0.9.1 Phase 6）：`ai_config` —— AIConfig 全量分片。
 *   密钥独立走 `save_ai_secret` / `delete_ai_secret` / `has_ai_secret` 命令，
 *   永不进 SQLite / IPC value 序列化路径。
 */

import { invoke } from "./tauri.js";

/**
 * 统一配置写入。
 * @param {string} key - 配置 key（见上方列表）
 * @param {*} value - 配置值（类型由 key 决定）
 * @returns {Promise<void>}
 */
export async function saveConfig(key, value) {
  return invoke("set_config", { key, value });
}
