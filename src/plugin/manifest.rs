//! 插件 manifest(JSON,见 production/0.2-core-plugin-design.md §3.3)。
//!
//! 本切片解析必要字段;permissions/resources/icon 等未列字段由 serde 默认忽略
//! (不建字段)。permissions 自用阶段完全不解析(§3.6)。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// core 当前支持的 manifest schema_version 上限(§3.7 B4:超出范围拒绝加载)。
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// 插件清单。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub id: String,
    #[allow(dead_code)] // 设置页展示用,本切片仅日志
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
    #[serde(default)]
    #[allow(dead_code)] // builtin 信任来源标记,本切片只加载 builtin
    pub builtin: bool,
    pub runtime: PluginRuntime,
    #[serde(default)]
    pub triggers: Vec<PluginTrigger>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// 进程拉起参数。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginRuntime {
    /// 可执行路径(相对 manifest 所在目录,或绝对路径)。
    pub exec: String,
    #[serde(default)]
    #[allow(dead_code)] // process/jsonl 本切片唯一形态,先解析不分支
    pub r#type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)] // 并发池 §3.7 B3 后续做
    pub concurrency: Option<u32>,
}

/// 触发器。本切片只实现 keyword(精确/前缀);regex 先定义不实现。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginTrigger {
    Keyword {
        keyword: String,
        #[serde(default = "default_exclusive")]
        #[allow(dead_code)] // 独占语义留给 RuleRouter(§4.3),本切片不消费
        exclusive: bool,
    },
    /// 正则触发(本切片定义不实现)。
    #[allow(dead_code)]
    Regex {
        pattern: String,
        #[serde(default = "default_exclusive")]
        exclusive: bool,
    },
}

fn default_exclusive() -> bool {
    true
}

impl PluginManifest {
    /// 从 manifest.json 路径解析。
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
        let manifest: PluginManifest =
            serde_json::from_str(&raw).map_err(|e| format!("解析失败: {e}"))?;
        if manifest.schema_version > SUPPORTED_SCHEMA_VERSION {
            return Err(format!(
                "schema_version {} 超出支持上限 {}",
                manifest.schema_version, SUPPORTED_SCHEMA_VERSION
            ));
        }
        Ok(manifest)
    }

    /// 是否提供 query 召回能力。
    pub fn supports_query(&self) -> bool {
        self.capabilities.iter().any(|c| c == "query")
    }

    /// 解析 exec 为绝对路径(相对路径基于 manifest 所在目录,不依赖 cwd)。
    pub fn exec_path(&self, manifest_dir: &Path) -> PathBuf {
        let exec = PathBuf::from(&self.runtime.exec);
        if exec.is_absolute() {
            exec
        } else {
            manifest_dir.join(exec)
        }
    }

    /// 查询超时(毫秒),缺省 3000。
    pub fn timeout_ms(&self) -> u64 {
        self.runtime.timeout_ms.unwrap_or(3000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "schema_version": 1,
        "id": "builtin.echo",
        "name": "Echo",
        "version": "0.1.0",
        "builtin": true,
        "runtime": { "type": "process", "protocol": "jsonl", "exec": "./bin/echo.exe", "timeout_ms": 1000 },
        "triggers": [{ "type": "keyword", "keyword": "echo", "exclusive": true }],
        "capabilities": ["query"],
        "permissions": ["clipboard"],
        "homepage": "https://example.com"
    }"#;

    #[test]
    fn parses_required_fields_ignores_unknown() {
        let m: PluginManifest = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(m.id, "builtin.echo");
        assert!(m.builtin);
        assert!(m.supports_query());
        assert_eq!(m.timeout_ms(), 1000);
        assert_eq!(m.triggers.len(), 1);
        // permissions / homepage 未建字段,被 serde 忽略,不报错
    }

    #[test]
    fn keyword_trigger_default_exclusive() {
        let json = r#"{"type":"keyword","keyword":"ip"}"#;
        let t: PluginTrigger = serde_json::from_str(json).unwrap();
        assert!(matches!(t, PluginTrigger::Keyword { exclusive: true, .. }));
    }

    #[test]
    fn missing_timeout_defaults_3000() {
        let json = r#"{"schema_version":1,"id":"x","name":"X","version":"0","runtime":{"exec":"x.exe"}}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.timeout_ms(), 3000);
    }
}
