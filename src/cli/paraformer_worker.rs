//! ParaformerOnline 常驻 worker 隐藏 CLI 入口（0.22.9 Handoff 07A）。
//!
//! 由 host launcher 通过 `blink.exe paraformer-worker --deployment <dir>` 启动。
//!
//! ## 设计铁则
//!
//! - 不出现在普通用户 CLI 帮助、功能目录、Capability、MCP 或前端中
//! - 参数只能由编译期 implementation 和冻结 deployment 构造
//! - 不允许前端注入 executable、DLL、模型路径、URL、argv 或环境变量
//! - 对传入 deployment 目录做受限目录、manifest 和 hash 校验
//! - stdout 只传 binary protocol，stderr 只写 tracing
//! - 与 paraformer-selftest 职责分离：self-test 是一次性验证，worker 是常驻

use std::path::PathBuf;

/// 解析 `--deployment <dir>` 参数。
fn parse_args(args: &[String]) -> Result<PathBuf, String> {
    let mut deployment = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--deployment" => {
                i += 1;
                deployment = args.get(i).map(PathBuf::from);
            }
            _ => {
                // 未知参数，忽略
            }
        }
        i += 1;
    }
    deployment.ok_or_else(|| "缺少 --deployment 参数".to_string())
}

/// 从 CLI 参数运行 worker。
///
/// 返回 exit code（0=成功退出，1=失败）。
pub fn run_from_args(args: &[String]) -> i32 {
    let deployment_dir = match parse_args(args) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("paraformer-worker: 参数解析失败: {e}");
            return 1;
        }
    };

    // 受限目录校验——deployment 必须存在且是目录
    if !deployment_dir.is_dir() {
        eprintln!(
            "paraformer-worker: deployment 目录不存在: {}",
            deployment_dir.display()
        );
        return 1;
    }

    crate::infra::local_engine::paraformer_worker::run_worker_loop(&deployment_dir)
}
