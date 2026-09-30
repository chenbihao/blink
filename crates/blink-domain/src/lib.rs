//! 领域层（0.25.4 自 bin crate 整体拆出）：搜索、意图、插件、能力协议主体。
//!
//! 依赖方向：blink-domain → {blink-infra, blink-domain-capability(error),
//! blink-domain-config(stt_config/store), blink-domain-local-engine,
//! blink-domain-stt}——不依赖 tauri/app（协议端口 `event::CapabilityEnv`
//! 隔离框架）。bin 侧 `src/domain/mod.rs` 为 re-export shim 保持旧路径。

pub mod ai;
pub mod capability; // 0.9.7：能力协议层（原子能力 + 统一声明/返回；error 在 blink-domain-capability）
pub mod chord;
pub mod clipboard; // 0.19.6：剪贴板读写共享语义（command / Capability 共用）
pub mod color; // 0.20.3：确定性颜色字面量解析（纯函数，Rust/JS 共享 fixture）
pub mod config; // 0.14.6 §2.1：配置域（stt_config/store 已拆 blink-domain-config）
pub mod context;
pub mod editor; // 0.23.1：内容编辑器域（SourceDescriptor/会话类型/纯决策，框架无关）
pub mod event; // 0.14.6 §2.2 / 0.21.14：领域环境抽象（EventPort + CapabilityEnv，domain 去 tauri）
pub mod event_names; // 0.21.14：事件名常量（真源在 blink-infra，此处 re-export）
pub mod feature_catalog; // 0.21.4：功能目录聚合层
pub mod intent;
pub mod mcp; // 0.13.0：MCP client（消费外部 tool，包装进 Tool 适配层）
pub mod ocr; // 0.22.4：OCR 领域协议（config/error/layout/types 已下沉 blink-infra）
pub mod palette; // 0.20.7：配色核心（OKLab/OKLCH/聚类/角色/搭配/对比度，Rust 单一真源）
pub mod plugin;
pub mod resource; // 0.23.12：统一资源 ref 层（ResourceStore 协议 + DefaultResourceStore）
pub mod schema; // 0.14.6：ToolSchema 公共基（CapabilitySchema 共享）
pub mod search;
pub mod sticky; // 0.16.7：桌面便签域（模型、服务、恢复）

// ── 架构守卫（0.25.4）────────────────────────────────────────────────────

#[cfg(test)]
mod arch_guard {
    /// blink-domain 不得 use tauri / windows crate（对齐 stt crate 守卫模式）。
    /// 端口抽象（event::CapabilityEnv）隔离框架；Win32 一律走 blink-infra。
    #[test]
    fn domain_does_not_use_tauri_or_windows_crate() {
        use std::fs;
        use std::path::{Path, PathBuf};

        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let src = Path::new(&manifest_dir).join("src");
        assert!(src.exists(), "src 目录应存在: {}", src.display());

        fn collect_rs(dir: &Path, files: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        collect_rs(&p, files);
                    } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                        files.push(p);
                    }
                }
            }
        }
        let mut files = Vec::new();
        collect_rs(&src, &mut files);
        assert!(!files.is_empty());

        // 运行时构造避免本测试自身触发。
        // 注意：search 模块有本地子模块名 windows（平台实现），故只匹配
        // windows::Win32 形态（真 crate 引用），不匹配裸 "use windows::"。
        let t1 = format!("use {}", "tauri");
        let t2 = format!("{}{}", "tauri", "::");
        let t3 = format!("{}{}", "windows", "::Win32");
        let forbidden = [t1, t2, t3];

        let mut violations = Vec::new();
        for f in &files {
            let content = match fs::read_to_string(f) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (i, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                for pat in &forbidden {
                    if trimmed.contains(pat) {
                        violations.push(format!("{}:{}: {}", f.display(), i + 1, trimmed));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "blink-domain 不得依赖 tauri 或 Win32 crate（平台调用走 blink-infra）。
违规:
{}",
            violations.join("
")
        );
    }
}
