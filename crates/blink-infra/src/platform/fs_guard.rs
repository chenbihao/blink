//! 文件内容读取前的受限路径防御（0.26.9）。
//!
//! 以**不触发内容下载、不跟随重解析点**的方式探测属性/目标，再决定是否
//! 允许读取内容：UNC、映射网络盘、需下载的云占位（RECALL_ON_DATA_ACCESS /
//! OFFLINE）与绕到远程/设备目标的重解析点默认拒绝自动内容读取——这些路径
//! 仍可保存/复制，只是预览与「存为图片条目」不读内容。设备命名空间路径在
//! 采集侧已被 `dropfiles::validate_single_path` 拒绝，读取侧在此再次防御。

/// 内容读取拒绝原因（稳定码供 UI 分类与日志；不含路径原文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// 设备命名空间（`\\.\` / `\\?\Volume{...}` 等）。
    DevicePath,
    /// UNC 路径（`\\server\share` / `\\?\UNC\...`）。
    Unc,
    /// 盘符映射到网络驱动器（GetDriveTypeW = DRIVE_REMOTE）。
    NetworkDrive,
    /// 目录（文件夹路径本身可保存，但不作为图片内容读取）。
    Directory,
    /// 云占位文件：RECALL_ON_DATA_ACCESS / OFFLINE 属性——读取会触发下载。
    CloudPlaceholder,
    /// 重解析点绕到远程目标。
    ReparseRemote,
    /// 其他/未知重解析点，保守拒绝。
    UnknownReparse,
    /// 文件不存在或属性不可访问。
    Missing,
}

impl DenyReason {
    /// 稳定原因码（`device_path` / `unc` / ...，前端据此映射文案）。
    pub fn reason(&self) -> &'static str {
        match self {
            DenyReason::DevicePath => "device_path",
            DenyReason::Unc => "unc",
            DenyReason::NetworkDrive => "network_drive",
            DenyReason::Directory => "directory",
            DenyReason::CloudPlaceholder => "cloud_placeholder",
            DenyReason::ReparseRemote => "reparse_remote",
            DenyReason::UnknownReparse => "unknown_reparse",
            DenyReason::Missing => "missing",
        }
    }
}

/// 探测结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentAccess {
    Allowed,
    Denied(DenyReason),
}

/// 探测路径是否允许读取文件内容；调用者须在有界阻塞通道执行。
pub fn probe_content_access(path: &str) -> ContentAccess {
    #[cfg(target_os = "windows")]
    {
        match probe_windows_path(std::path::Path::new(path), 0, true) {
            Ok(()) => ContentAccess::Allowed,
            Err(reason) => ContentAccess::Denied(reason),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        ContentAccess::Denied(DenyReason::Missing)
    }
}

/// 是否为 UNC 路径（`\\server\share` 或 `\\?\UNC\...`）。
fn is_unc_path(path: &str) -> bool {
    if path.starts_with(r"\\?\UNC\") {
        return true;
    }
    path.starts_with(r"\\") && !path.starts_with(r"\\?\") && !path.starts_with(r"\\.\")
}

/// 打开后再次按句柄核对真实目标与属性，后续必须读取此句柄，避免路径替换竞态。
pub fn open_content_file(path: &str) -> Result<std::fs::File, DenyReason> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        };
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                FILE_FLAG_OPEN_NO_RECALL, FILE_NAME_NORMALIZED, GETFINALPATHNAMEBYHANDLE_FLAGS,
                GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
            },
        };

        probe_windows_path(std::path::Path::new(path), 0, true)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_NO_RECALL.0)
            .open(path)
            .map_err(|_| DenyReason::Missing)?;
        let metadata = file.metadata().map_err(|_| DenyReason::Missing)?;
        check_attributes(metadata.file_attributes(), true)?;
        let mut final_path = vec![0u16; 32_768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(file.as_raw_handle()),
                &mut final_path,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
            )
        } as usize;
        if length == 0 || length >= final_path.len() {
            return Err(DenyReason::UnknownReparse);
        }
        let final_path =
            String::from_utf16(&final_path[..length]).map_err(|_| DenyReason::UnknownReparse)?;
        check_path_location(&final_path, is_network_drive(&final_path))?;
        Ok(file)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(DenyReason::Missing)
    }
}

fn check_path_location(path: &str, network_drive: bool) -> Result<(), DenyReason> {
    if crate::platform::clipboard::dropfiles::is_device_namespace(path) {
        return Err(DenyReason::DevicePath);
    }
    if is_unc_path(path) {
        return Err(DenyReason::Unc);
    }
    if !crate::platform::clipboard::dropfiles::is_absolute_fs_path(path) {
        return Err(DenyReason::UnknownReparse);
    }
    if network_drive {
        return Err(DenyReason::NetworkDrive);
    }
    Ok(())
}

fn check_attributes(attrs: u32, final_component: bool) -> Result<(), DenyReason> {
    // OFFLINE / RECALL_ON_OPEN / RECALL_ON_DATA_ACCESS：不能通过读取触发物化。
    if attrs & (0x1000 | 0x40000 | 0x400000) != 0 {
        return Err(DenyReason::CloudPlaceholder);
    }
    if final_component && attrs & 0x10 != 0 {
        return Err(DenyReason::Directory);
    }
    Ok(())
}

/// 从根向叶检查每个重解析点；相对链接以链接父目录解析，链深度有界。
#[cfg(target_os = "windows")]
fn probe_windows_path(
    path: &std::path::Path,
    depth: usize,
    require_file: bool,
) -> Result<(), DenyReason> {
    use std::os::windows::fs::MetadataExt;
    if depth >= 16 {
        return Err(DenyReason::UnknownReparse);
    }
    let text = path.to_str().ok_or(DenyReason::UnknownReparse)?;
    check_path_location(text, is_network_drive(text))?;
    let components: Vec<_> = path.ancestors().collect();
    for component in components.into_iter().rev() {
        let metadata = std::fs::symlink_metadata(component).map_err(|_| DenyReason::Missing)?;
        let attrs = metadata.file_attributes();
        check_attributes(attrs, false)?;
        if attrs & 0x400 != 0 {
            // FILE_ATTRIBUTE_REPARSE_POINT
            let target = std::fs::read_link(component).map_err(|_| DenyReason::UnknownReparse)?;
            let target = if target.is_absolute() {
                target
            } else {
                component
                    .parent()
                    .ok_or(DenyReason::UnknownReparse)?
                    .join(target)
            };
            probe_windows_path(&target, depth + 1, false).map_err(|reason| match reason {
                DenyReason::Unc | DenyReason::NetworkDrive => DenyReason::ReparseRemote,
                other => other,
            })?;
        }
        if require_file && component == path {
            check_attributes(attrs, true)?;
        }
    }
    Ok(())
}

/// 盘符是否映射到网络驱动器（DRIVE_REMOTE = 4）。
#[cfg(target_os = "windows")]
fn is_network_drive(path: &str) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::PCWSTR;

    // 归一化 `\\?\C:\...` → `C:\`，普通 `C:\...` → `C:\`
    let stripped = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = stripped.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || (bytes[2] != b'\\' && bytes[2] != b'/')
    {
        return false; // 非盘符路径（UNC 已在前面拦截），保守不判网络盘
    }
    let root: String = stripped.chars().take(3).collect();
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) == 4 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_paths_are_denied_before_any_io() {
        assert_eq!(
            probe_content_access(r"\\server\share\a.png"),
            ContentAccess::Denied(DenyReason::Unc)
        );
        assert_eq!(
            probe_content_access(r"\\?\UNC\server\share\a.png"),
            ContentAccess::Denied(DenyReason::Unc)
        );
    }

    #[test]
    fn device_paths_are_denied_again_on_read_side() {
        assert_eq!(
            probe_content_access(r"\\.\PhysicalDrive0"),
            ContentAccess::Denied(DenyReason::DevicePath)
        );
        assert_eq!(
            probe_content_access(r"\\?\Volume{12345678-0000-0000-0000-000000000000}\a"),
            ContentAccess::Denied(DenyReason::DevicePath)
        );
    }

    #[test]
    fn deny_reason_codes_are_stable() {
        assert_eq!(DenyReason::Unc.reason(), "unc");
        assert_eq!(DenyReason::CloudPlaceholder.reason(), "cloud_placeholder");
        assert_eq!(DenyReason::NetworkDrive.reason(), "network_drive");
        assert_eq!(DenyReason::Directory.reason(), "directory");
        assert_eq!(DenyReason::Missing.reason(), "missing");
    }

    #[test]
    fn is_unc_path_classification() {
        assert!(is_unc_path(r"\\server\share\a"));
        assert!(is_unc_path(r"\\?\UNC\server\share\a"));
        assert!(!is_unc_path(r"C:\a"));
        assert!(!is_unc_path(r"\\?\C:\a"));
        assert!(!is_unc_path(r"\\.\COM1"));
    }

    #[test]
    fn resolved_targets_and_recall_attributes_are_rechecked() {
        assert_eq!(
            check_path_location(r"Z:\image.png", true),
            Err(DenyReason::NetworkDrive)
        );
        assert_eq!(
            check_path_location(r"\\?\UNC\host\share\image.png", false),
            Err(DenyReason::Unc)
        );
        assert_eq!(check_path_location(r"\\?\C:\image.png", false), Ok(()));
        for attrs in [0x1000, 0x40000, 0x400000] {
            assert_eq!(
                check_attributes(attrs, false),
                Err(DenyReason::CloudPlaceholder)
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn local_link_chain_and_parent_links_use_checked_file_handle() {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("image.png");
        std::fs::write(&original, b"original").unwrap();
        let link = dir.path().join("image-link.png");
        if symlink_file(&original, &link).is_err() {
            return; // Windows 未启用开发者模式/符号链接权限时跳过平台样本。
        }
        assert!(open_content_file(link.to_str().unwrap()).is_ok());
        let parent_link = dir.path().join("parent-link");
        symlink_dir(dir.path(), &parent_link).unwrap();
        assert!(open_content_file(parent_link.join("image.png").to_str().unwrap()).is_ok());
        std::fs::remove_dir(&parent_link).unwrap();
        std::fs::remove_file(&link).unwrap();
        // 自指链不能无限递归。
        symlink_file(&link, &link).unwrap();
        assert_eq!(
            probe_content_access(link.to_str().unwrap()),
            ContentAccess::Denied(DenyReason::UnknownReparse)
        );
    }
}
