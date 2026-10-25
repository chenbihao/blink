//! 插件系统(0.3,见 phases/0.2-core-plugin-design.md §3)。
//!
//! 本切片:builtin 插件加载(扫描 manifest)+ 进程拉起(JSONL stdio)+ PluginEngine
//! 接 async lane。第三方插件目录(%APPDATA%\blink\plugins)、permissions、热重载等后续。

mod capability;
mod contract;
mod direction;
mod engine;
mod install;
mod manifest;
mod runtime;
mod process;
mod protocol;
mod secret_update;
mod weather_config;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use capability::{PluginCapabilityAdapter, plugin_tool_id};
pub use contract::{
    PluginContract, BUILTIN_NAMESPACE, validate_contract_set, validate_namespace,
    validate_plugin_id,
};
pub use direction::{SUPPORTED_TARGETS, lang_display_name, resolve_display_target};
pub use engine::PluginEngine;
pub use install::{
    InstallError, PluginInstaller, UninstallOutcome, compare_versions, current_platform_tag,
};
pub use manifest::{
    ArgHint, DatasetDef, HostCompat, LocalizableText, ManifestContextWhen, ManifestSurfaceHint,
    PageDef, PluginManifest, PluginTrigger,
};
pub use process::PluginHandle;
pub use runtime::{PluginRuntimePublisher, PublishReport};
#[allow(unused_imports)]
pub use protocol::{
    PluginDirs, PluginQueryContext, PluginRawResult, RawToolResult, SUPPORTED_PROTOCOL_VERSION,
};

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
///
/// 0.14.6 §2.2：`AppHandle` 参数移除，由调用方（main.rs）解析路径后传入。
pub fn builtin_plugins_dir() -> PathBuf {
    if cfg!(debug_assertions) {
        // 0.25.4 拆 crate 后，编译期目录是 crates/blink-domain，需上溯到仓库根。
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("blink-domain 应位于 workspace 的 crates/ 下")
            .join("plugins")
            .join("builtin")
    } else {
        PathBuf::from("plugins").join("builtin")
    }
}

/// 示例插件目录（仓库内 `plugins/examples`，**仅 dev 加载**）。
///
/// 0.25.20：echo-python / echo-node 脚本示例插件在此验证托管解释器链路；
/// release 包的 bundle resources 只含 `plugins/builtin/**/*`，examples
/// 不随包分发——它们是插件作者的参考模板与开发期链路验证，不是产品功能。
#[cfg(debug_assertions)]
pub fn examples_plugins_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("blink-domain 应位于 workspace 的 crates/ 下")
        .join("plugins")
        .join("examples")
}

/// 加载一个目录下的全部插件:扫描 `<dir>/*/manifest.json` → 解析 → 冻结契约
/// → 集合校验。失败的单个插件跳过(降级,不影响其余),记日志；**契约集合校验
/// 失败的插件剔除后重试**（碰撞双方取加载序在先者），不会整目录报废。
/// proxy=(http_proxy, https_proxy)，进程启动时 env 注入，ureq/reqwest 原生读取。
///
/// 0.28.1：不再要求 `capabilities` 含 `query`——纯 tool（AI 能力）与纯页面
/// 插件均能加载；三态皆无（数据包目录）仍跳过。
///
/// 0.14.6 §2.2：不再接收 `&AppHandle`，改为接收 `plugins_dir: &Path`。
pub fn load_builtin_plugins(
    plugins_dir: &Path,
    proxy: Option<(String, String)>,
) -> Vec<Arc<PluginHandle>> {
    let dir = plugins_dir.to_path_buf();
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        tracing::debug!(dir = %dir.display(), "无 builtin 插件目录,跳过");
        return Vec::new();
    };

    let mut loaded: Vec<(Arc<PluginHandle>, PathBuf)> = Vec::new();
    for entry in read_dir.flatten() {
        let plugin_dir = entry.path();
        let manifest_path = plugin_dir.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        match PluginManifest::from_path(&manifest_path) {
            Ok(m) if m.supports_query() || !m.tools.is_empty() || m.supports_pages() => {
                // 0.28.1：单插件契约冻结（id/exec/页面 id 合法性），
                // 失败按单插件降级跳过，不阻塞目录内其他插件。
                match PluginContract::from_manifest(&m) {
                    Ok(_contract) => {
                        tracing::info!(plugin = %m.id, "已加载插件 manifest");
                        loaded.push((
                            Arc::new(PluginHandle::new(Arc::new(m), plugin_dir.clone(), proxy.clone())),
                            plugin_dir,
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %manifest_path.display(),
                            error = %e,
                            "插件契约校验失败,跳过"
                        );
                    }
                }
            }
            Ok(m) => {
                tracing::debug!(plugin = %m.id, "插件无 query/tool/页面能力,跳过");
            }
            Err(e) => {
                tracing::warn!(path = %manifest_path.display(), error = %e, "插件 manifest 加载失败");
            }
        }
    }

    // 0.28.1：集合级契约校验（plugin id 重复 / tool id 映射碰撞 / 关键词抢占）。
    // builtin 加载先于 CapabilityRegistry 构造，与既有注册表的碰撞由注册点
    // （main.rs get() 预检 + 运行期 publish 校验）承担；此处保证目录内自洽。
    // 整体校验通过走快路径；失败时增量重放——碰撞双方保留加载序在先者。
    let contracts: Vec<PluginContract> = loaded
        .iter()
        .filter_map(|(h, _)| PluginContract::from_manifest(h.manifest()).ok())
        .collect();
    if validate_contract_set(&contracts, &Default::default(), &Default::default()).is_ok() {
        return loaded.into_iter().map(|(h, _)| h).collect();
    }
    tracing::warn!("插件目录存在契约冲突，按加载序保留在先者，剔除后到冲突插件");
    let mut kept: Vec<Arc<PluginHandle>> = Vec::with_capacity(loaded.len());
    let mut kept_contracts: Vec<PluginContract> = Vec::with_capacity(loaded.len());
    for (handle, dir) in loaded {
        let Ok(c) = PluginContract::from_manifest(handle.manifest()) else {
            continue;
        };
        let mut trial = kept_contracts.clone();
        trial.push(c);
        if validate_contract_set(&trial, &Default::default(), &Default::default()).is_ok()
        {
            kept.push(handle);
            kept_contracts = trial;
        } else {
            tracing::warn!(plugin_dir = %dir.display(), "插件与已加载集合契约冲突,剔除");
        }
    }
    kept
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn dev_discovers_builtin_plugins_and_translation_tools() {
        // 实际扫描仓库 manifest，防止拆 crate 后路径漂移但存量解析单测仍通过。
        // PluginHandle 懒启动，此处不会拉起插件进程或读取用户配置。
        let plugins = load_builtin_plugins(&builtin_plugins_dir(), None);
        let mut ids: Vec<_> = plugins.iter().map(|plugin| plugin.id()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["builtin.ip", "builtin.translate", "builtin.weather"]);

        let translate = plugins
            .iter()
            .find(|plugin| plugin.id() == "builtin.translate")
            .expect("应发现翻译插件");
        for tool_name in ["translate", "translate_batch"] {
            assert!(
                translate
                    .manifest()
                    .tools
                    .iter()
                    .any(|tool| tool.name == tool_name),
                "翻译插件应声明 {tool_name} 能力"
            );
        }
    }

    /// 0.25.20：examples 目录的脚本示例插件（echo-python / echo-node）必须
    /// 可被发现且 manifest 可解析——这是托管解释器链路的 dev 验证载体，
    /// 示例腐烂（manifest 改名/字段变更）在此处早期暴露。
    #[test]
    fn dev_discovers_example_script_plugins() {
        let plugins = load_builtin_plugins(&examples_plugins_dir(), None);
        let mut ids: Vec<_> = plugins.iter().map(|plugin| plugin.id()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["example.echo-node", "example.echo-python"]);
    }

    /// 0.28.1：纯 tool / 纯页面插件（无 query capability）必须能加载；
    /// tool id 点转下划线映射碰撞的后到者被剔除，先到者保留。
    #[test]
    fn loads_pure_tool_and_page_plugins_and_drops_collision() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mk = |dir: &str, json: String| {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("manifest.json"), json).unwrap();
        };
        // 纯 tool 插件（无 capabilities.query；tool 型必须声明 exec）
        mk(
            "toolonly",
            r#"{"schema_version":1,"id":"extra.toolonly","name":"T","version":"0.1",
                "runtime":{"exec":"t.exe"},
                "tools":[{"name":"run"}]}"#
                .replace('\n', ""),
        );
        // 纯页面插件（无 runtime）
        mk(
            "pageonly",
            r#"{"schema_version":1,"id":"extra.pageonly","name":"P","version":"0.1",
                "pages":[{"id":"main","entry":"index.html"}]}"#
                .replace('\n', ""),
        );
        // 与 extra.toolonly 的 run 映射碰撞：extra + toolonly_run
        mk(
            "collide",
            r#"{"schema_version":1,"id":"extra","name":"C","version":"0.1",
                "runtime":{"exec":"c.exe"},
                "tools":[{"name":"toolonly_run"}]}"#
                .replace('\n', ""),
        );

        let plugins = load_builtin_plugins(root, None);
        let mut ids: Vec<_> = plugins.iter().map(|p| p.id().to_string()).collect();
        ids.sort_unstable();
        // 碰撞双方（extra / extra.toolonly 同映射 extra_toolonly_run）只存活其一
        // ——目录读取序不定，不假设哪一方在先；无关插件不受影响。
        assert!(ids.contains(&"extra.pageonly".to_string()), "纯页面插件应加载: {ids:?}");
        let survived = ids.iter().filter(|id| id.as_str() == "extra" || id.as_str() == "extra.toolonly").count();
        assert_eq!(survived, 1, "碰撞双方应只存活其一: {ids:?}");
    }
}
