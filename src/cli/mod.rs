//! CLI 模块（0.13.5）——Blink as CLI。
//!
//! 让 Blink 的能力不限于 GUI 窗口，也能在终端 / 脚本 / 自动化场景中使用。
//!
//! ## 架构
//!
//! CLI 和 GUI 共用 `src/domain/` 和 `src/app/` 的函数，不重复实现。
//! CLI 模式下不创建 Tauri 窗口，但创建 AppHandle（能力通过它访问 managed state）。
//!
//! ## 命令
//!
//! - `blink mcp-server` — 作为 MCP server 运行（stdio 模式，0.13.4）
//! - `blink search <query>` — 应用搜索
//! - `blink run <capability> [--args JSON]` — 调用任意 Capability
//! - `blink config get <key>` / `blink config set <key> <value>` — 读写配置
//! - `blink capabilities` — 列出所有可用 Capability
//! - `blink chat [--model <id>]` — 终端对话模式（基础实现）
//! - `blink help` / `blink --help` — 显示帮助（clap 自动生成）

pub mod commands;

use clap::{Parser, Subcommand};

/// Blink CLI 入口。
#[derive(Parser, Debug)]
#[command(name = "blink", version, about = "Blink — Windows 全局快捷入口 (CLI 模式)", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// CLI 子命令。
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 作为 MCP server 运行（stdio 模式，供外部 MCP client 连接）
    McpServer,

    /// 搜索应用
    Search {
        /// 搜索关键词
        query: String,
        /// 输出 JSON 格式（供脚本消费）
        #[arg(long)]
        json: bool,
    },

    /// 调用任意 Capability
    Run {
        /// Capability id（如 capture_screen / search_files）
        capability: String,
        /// 参数 JSON（如 '{"query": "test"}'）
        #[arg(long)]
        args: Option<String>,
    },

    /// 列出所有可用 Capability
    Capabilities {
        /// 输出 JSON 格式
        #[arg(long)]
        json: bool,
    },

    /// 读写配置
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// 终端对话模式（基础实现）
    Chat {
        /// 模型 id（如 gpt-4o-mini），缺省用默认模型
        #[arg(long)]
        model: Option<String>,
        /// 对话 id（继续已有对话），缺省新建
        #[arg(long)]
        conversation: Option<String>,
    },
}

/// 配置子命令。
#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// 读取配置项
    Get {
        /// 配置 key（如 mcp:server / mcp:servers）
        key: String,
    },
    /// 写入配置项
    Set {
        /// 配置 key
        key: String,
        /// 配置 value（JSON 字符串）
        value: String,
    },
}

/// CLI 入口分发——检测 CLI 模式并执行对应命令。
///
/// 返回 `Some(exit_code)` 表示已处理 CLI 命令（main 应直接 exit）；
/// 返回 `None` 表示非 CLI 模式（main 应继续启动 GUI）。
pub fn try_run_cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();

    // 没有子命令参数 → GUI 模式
    if args.len() < 2 {
        return None;
    }

    // 第一个参数是子命令（不是 flag）
    let first = &args[1];
    let known_commands = [
        "mcp-server",
        "search",
        "run",
        "capabilities",
        "config",
        "chat",
        "help", // 0.13.7: 支持 blink help / blink --help
    ];
    // --help / -h 也走 CLI 路径（clap 自动处理）
    if first == "--help" || first == "-h" || first == "--version" || first == "-V" {
        // 让 clap 处理——必须显式打印 help/version 文本到 stdout，
        // 否则 subprocess 捕获不到输出（`try_parse` 返回 Err 但不自动打印）
        match Cli::try_parse() {
            Ok(_) => return Some(0),
            Err(e) => {
                // clap 的 DisplayHelp/DisplayVersion 写 stdout，其他错误写 stderr
                let _ = e.print();
                return Some(0);
            }
        }
    }
    if !known_commands.contains(&first.as_str()) {
        return None;
    }

    // 解析 CLI 参数
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("{e}");
            return Some(1);
        }
    };

    // 执行 CLI 命令（阻塞，内部创建 Tauri app + tokio runtime）
    let exit_code = commands::dispatch(cli);
    Some(exit_code)
}
