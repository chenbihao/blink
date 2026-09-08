//! GPU 能力检测（CUDA / Vulkan）。
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

/// 判断已检测到的 CUDA 驱动版本是否满足声明的最低版本。
pub fn cuda_meets_min_version(min_version: Option<&str>) -> bool {
    let Some(actual) = detect_cuda() else {
        return false;
    };
    let Some(minimum) = min_version else {
        return true;
    };
    compare_driver_version(&actual, minimum) >= std::cmp::Ordering::Equal
}

fn compare_driver_version(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |value: &str| -> Vec<u32> {
        value
            .split(|c: char| !c.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .take(3)
            .collect()
    };
    let mut lhs = parse(left);
    let mut rhs = parse(right);
    lhs.resize(3, 0);
    rhs.resize(3, 0);
    lhs.cmp(&rhs)
}

/// 受控 Vulkan loader/API 预筛。
///
/// 不调用用户安装的 `vulkaninfo`，也不把预筛结果当成 worker 的权威
/// probe。这里只确认系统 loader 可加载并导出 Vulkan 的入口点，减少把
/// 明显不兼容的 profile 送入安装事务。
#[cfg(windows)]
pub fn detect_vulkan() -> Option<String> {
    static DETECTED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DETECTED.get_or_init(detect_vulkan_uncached).clone()
}

#[cfg(windows)]
fn detect_vulkan_uncached() -> Option<String> {
    use std::ffi::OsStr;
    use std::mem::transmute;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{PCSTR, PCWSTR};

    let wide: Vec<u16> = OsStr::new("vulkan-1.dll")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())).ok()? };
    unsafe {
        let enumerate = GetProcAddress(
            module,
            PCSTR(c"vkEnumerateInstanceVersion".as_ptr() as *const u8),
        );
        if let Some(proc) = enumerate {
            type EnumerateInstanceVersion = unsafe extern "system" fn(*mut u32) -> i32;
            let enumerate: EnumerateInstanceVersion = transmute(proc);
            let mut version = 0u32;
            if enumerate(&mut version) == 0 {
                Some(format!(
                    "{}.{}.{}",
                    version >> 22,
                    (version >> 12) & 0x3ff,
                    version & 0xfff
                ))
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
pub fn detect_vulkan() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::compare_driver_version;

    #[test]
    fn cuda_version_comparison_is_numeric() {
        assert!(compare_driver_version("12.10", "12.2").is_gt());
        assert!(compare_driver_version("12.2", "12.2").is_eq());
        assert!(compare_driver_version("11.8", "12.0").is_lt());
    }
}
