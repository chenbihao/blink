//! 插件系统(0.3,见 production-design/phases/0.2-core-plugin-design.md §3)。
//!
//! 本切片:builtin 插件加载(扫描 manifest)+ 进程拉起(JSONL stdio)+ PluginEngine
//! 接 async lane。第三方插件目录(%APPDATA%\blink\plugins)、permissions、热重载等后续。

mod engine;
mod manifest;
mod process;
mod protocol;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Manager};

pub use engine::PluginEngine;
pub use manifest::{LocalizableText, ManifestContextWhen, ManifestSurfaceHint, PluginManifest, PluginTrigger};
pub use process::{InterpretersStatus, PluginHandle, probe_interpreters};
pub use protocol::PluginQueryContext;

/// 插件配置读取抽象（0.8.2 §3.4.1 + 0.8.3 §4.13 扩展）。
///
/// `RuleRouter` 需要在解析 `ContextTrigger::TextIsNonTargetLang` 时读翻译插件的
/// `target_lang` 字段。为避免 `domain/intent` → `domain/plugin` 的正向依赖导致
/// 循环（`plugin::engine` 已经依赖 `intent::Route`），走 trait 反转：
/// 生产者 `PluginEngine` 实现 trait，消费者 `RuleRouter` 只知道 `Arc<dyn PluginSettingResolver>`。
///
/// 0.8.3 §4.13 P1 决策「禁用联动运行时查启用态」：加 `is_enabled` 让 `RuleRouter::best_suggestion`
/// 在产 Context Suggestion 前检查插件启用态,未启用则跳过 binding。默认实现返回 true（保持
/// 0.8.2 单测的兼容——mock resolver 不需要重写）。
pub trait PluginSettingResolver: Send + Sync {
    /// 读插件某个 settings 字段（字符串）。
    ///
    /// 返回 `None` 的场景：
    /// - 插件未加载 / 未启用
    /// - `settings` 为 null
    /// - `key` 不存在
    /// - 该字段值不是字符串
    fn get_string(&self, plugin_id: &str, key: &str) -> Option<String>;

    /// 插件是否启用（0.8.3 §4.13 P1）。默认 true——单测里的 mock 无需重写；
    /// 生产环境 `PluginEngine` 实现委托到自身 `is_enabled`。
    fn is_enabled(&self, _plugin_id: &str) -> bool {
        true
    }

    /// 读插件本地化显示名（0.8.3 §4.13 P0 修订）——`build_context_suggestion_text`
    /// 用来生成 Ghost display 文本（`翻译 "hello..."` 而不是 `translate "hello..."`）。
    ///
    /// 生产环境 `PluginEngine` 实现走 `manifest.name.resolve(lang)`；默认返回 None
    /// 让调用方 fallback 到 id 末段（单测 mock resolver 无需重写）。
    fn get_display_name(&self, _plugin_id: &str, _lang: &str) -> Option<String> {
        None
    }
}

/// builtin 插件根目录。
/// - debug:仓库内 `plugins/builtin`(开发期直接用 target 下编译出的插件 exe)。
/// - release:app 资源目录下的 `plugins/builtin`。
fn builtin_plugins_dir(app: &AppHandle) -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join("builtin")
    } else {
        app.path()
            .resource_dir()
            .map(|d| d.join("plugins").join("builtin"))
            .unwrap_or_else(|_| PathBuf::from("plugins").join("builtin"))
    }
}

/// 加载所有 builtin 插件:扫描 `<dir>/*/manifest.json` → 解析 → 过滤 query 能力。
/// 失败的单个插件跳过(降级,不影响其余),记日志。返回懒启动的 PluginHandle。
/// proxy=(http_proxy, https_proxy)，进程启动时 env 注入，ureq/reqwest 原生读取。
pub fn load_builtin_plugins(app: &AppHandle, proxy: Option<(String, String)>) -> Vec<Arc<PluginHandle>> {
    let dir = builtin_plugins_dir(app);
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        tracing::debug!(dir = %dir.display(), "无 builtin 插件目录,跳过");
        return Vec::new();
    };

    let mut loaded = Vec::new();
    for entry in read_dir.flatten() {
        let plugin_dir = entry.path();
        let manifest_path = plugin_dir.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        match PluginManifest::from_path(&manifest_path) {
            Ok(m) if m.supports_query() => {
                tracing::info!(plugin = %m.id, "已加载插件 manifest");
                loaded.push(Arc::new(PluginHandle::new(Arc::new(m), plugin_dir, proxy.clone())));
            }
            Ok(m) => {
                tracing::debug!(plugin = %m.id, "插件无 query 能力,跳过");
            }
            Err(e) => {
                tracing::warn!(path = %manifest_path.display(), error = %e, "插件 manifest 加载失败");
            }
        }
    }
    loaded
}
