//! 锁屏——Win32 `LockWorkStation` 封装（0.14.6 §2.3 从 domain/execution/builtin.rs 迁入）。
//!
//! domain 只调此函数，不直接 `use windows::`。

/// 锁定 Windows 工作站。成功返回 true。
///
/// 非 Windows 平台无操作，返回 false。
#[cfg(target_os = "windows")]
pub fn lock_workstation() -> bool {
    use windows::Win32::System::Shutdown::LockWorkStation;
    unsafe { LockWorkStation().is_ok() }
}

#[cfg(not(target_os = "windows"))]
pub fn lock_workstation() -> bool {
    false
}
