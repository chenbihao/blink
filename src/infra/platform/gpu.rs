//! GPU 能力检测（CUDA）。
//!
//! 0.22.10：原 `platform::python`（uv/venv 自管理）随 PythonVenv Provider
//! 一并退役，仅保留 `detect_cuda`——ManagedBinary/ONNX provider 的
//! `RequiresCuda` 兼容性检查仍依赖它。

use std::process::Command;

use super::no_window;

/// 检测系统是否有 NVIDIA GPU 及 CUDA 版本。
///
/// 通过运行 `nvidia-smi` 并解析输出中的 CUDA 版本。
/// 兼容新旧驱动格式：
/// - 旧：`CUDA Version: 12.2`
/// - 新：`CUDA UMD Version: 13.3`
///
/// 返回 CUDA 版本字符串（如 "12.2" / "13.3"），无 GPU 时返回 None。
pub fn detect_cuda() -> Option<String> {
    let output = no_window(Command::new("nvidia-smi")).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // 匹配 "CUDA Version: X.Y" 或 "CUDA UMD Version: X.Y"
        if line.contains("CUDA") && line.contains("Version:") {
            // 取 "Version:" 后面的版本号
            if let Some(idx) = line.find("Version:") {
                let rest = &line[idx + "Version:".len()..];
                // 跳过空格，取第一个数字串（如 "12.2" 或 "13.3"）
                let version = rest.split_whitespace().next()?.trim_end_matches('|').trim();
                if !version.is_empty()
                    && version
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                {
                    return Some(version.to_string());
                }
            }
        }
    }
    None
}
