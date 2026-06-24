//! 插件系统(0.3,见 production/0.2-core-plugin-design.md §3)。
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
pub use manifest::{PluginManifest, PluginTrigger};
pub use process::PluginHandle;
pub use protocol::PluginQueryContext;

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
pub fn load_builtin_plugins(app: &AppHandle) -> Vec<Arc<PluginHandle>> {
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
                loaded.push(Arc::new(PluginHandle::new(Arc::new(m), plugin_dir)));
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
