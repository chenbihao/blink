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
//! - `blink mcp-server` — 已迁移到主进程 Streamable HTTP（0.19.13，返回迁移提示）
//! - `blink search <query>` — 应用搜索
//! - `blink run <capability> [--args JSON]` — 调用任意 Capability
//! - `blink config get <key>` / `blink config set <key> <value>` — 读写配置
//! - `blink capabilities` — 列出所有可用 Capability
//! - `blink chat [--model <id>]` — 终端对话模式（基础实现）
//! - `blink help` / `blink --help` — 显示帮助（clap 自动生成）

pub mod commands;
pub mod onnx_validate;

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
    /// 已迁移到主进程 Streamable HTTP（0.19.13）。
    ///
    /// 旧 stdio 子进程路径已收口，执行时返回迁移提示。
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
        /// Capability id（如 screenshot / search_files）
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

    /// ONNX 隔离验证（0.22.8-B 隐藏入口，不在 help 中显示）。
    ///
    /// 由 OnnxRuntimeProvider 的 self_test 通过子进程调用，
    /// 加载 staging DLL + 创建 ORT Session + 执行最小推理。
    /// 不得出现在普通用户 CLI/help 中，也不得成为常驻 OCR worker。
    #[command(hide = true)]
    OnnxValidate {
        /// ORT DLL 路径
        #[arg(long)]
        dll: String,
        /// det 模型路径
        #[arg(long)]
        det: String,
        /// rec 模型路径
        #[arg(long)]
        rec: String,
        /// dictionary 路径
        #[arg(long)]
        dict: String,
        /// ORT intra_op 线程数
        #[arg(long)]
        intra_op: u32,
        /// ORT inter_op 线程数
        #[arg(long)]
        inter_op: u32,
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
    // onnx-validate 是隐藏入口，不在 known_commands 中列出，
    // 但需要被正确分派到 CLI 路径
    if first == "onnx-validate" {
        // 直接解析并执行，不走 clap 的完整子命令分派
        // （因为 onnx-validate 是隐藏的，不在 help 中显示）
        return Some(onnx_validate::run_from_args(&args[2..]));
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
