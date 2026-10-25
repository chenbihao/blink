//! 插件页面静态资源解析（0.28.2 §五.1）——`blink-plugin` 协议的路径安全层。
//!
//! 职责：把「插件包根目录 + 相对路径」解析为可服务的绝对文件路径，
//! 拒绝路径穿越 / 绝对路径 / 盘符 / 设备名 / 目录读取。纯函数，无 IO 副作用
//! （存在性校验由调用方决定用 `validate_entry` 还是返回 404）。
//!
//! 业务规则（谁能在协议上被服务）在 app 层协议处理器；本模块只管"给定根，
//! 这个相对路径是否安全、落在哪"。

use std::path::{Path, PathBuf};

/// 相对路径解析错误。
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PageResourceError {
    #[error("路径为空")]
    Empty,
    #[error("不接受绝对路径")]
    Absolute,
    #[error("不接受 .. 穿越")]
    Traversal,
    #[error("路径含 Windows 保留设备名")]
    ReservedDevice,
}

/// 解析插件包内相对资源路径。
///
/// 规则：
/// - 空串拒绝；`/`、`\` 开头与盘符路径拒绝（绝对路径）；
/// - 分量中 `..` 拒绝；`.` 与空分量跳过；
/// - Windows 保留设备名（CON/PRN/AUX/NUL/COM1..9/LPT1..9）分量拒绝；
/// - 正反斜杠都接受（web 资源作者两种都可能用）。
///
/// 返回 lexical join 的结果（未 canonicalize——服务时由调用方做存在性 +
/// 前缀复核；安装/打开入口可用 [`validate_entry`] 做带 IO 的完整校验）。
pub fn resolve_page_resource(
    package_root: &Path,
    rel_path: &str,
) -> Result<PathBuf, PageResourceError> {
    let trimmed = rel_path.trim();
    if trimmed.is_empty() {
        return Err(PageResourceError::Empty);
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err(PageResourceError::Absolute);
    }
    // 盘符绝对路径（C:\ 或 C:/）——Path::is_absolute 在拆分量前先拦
    if Path::new(trimmed).is_absolute() {
        return Err(PageResourceError::Absolute);
    }

    let mut out = package_root.to_path_buf();
    for raw in trimmed.split(['/', '\\']) {
        if raw.is_empty() || raw == "." {
            continue;
        }
        if raw == ".." {
            return Err(PageResourceError::Traversal);
        }
        if is_reserved_device_name(raw) {
            return Err(PageResourceError::ReservedDevice);
        }
        out.push(raw);
    }
    Ok(out)
}

/// 带 IO 的入口校验（安装/打开窗口时用）：解析 + 文件存在 + canonicalize
/// 后仍在包根内（防符号链接逃逸）。不存在/非文件统一报 Empty（调用方表现为
/// 「页面缺失」），越界报 Traversal。
pub fn validate_entry(package_root: &Path, entry: &str) -> Result<PathBuf, PageResourceError> {
    let candidate = resolve_page_resource(package_root, entry)?;
    let missing = || PageResourceError::Empty;
    if !candidate.is_file() {
        return Err(missing());
    }
    // 符号链接逃逸防御：canonicalize 后必须仍在 canonical 根内
    let canon_root = package_root.canonicalize().map_err(|_| missing())?;
    let canon_candidate = candidate.canonicalize().map_err(|_| missing())?;
    if !canon_candidate.starts_with(&canon_root) {
        return Err(PageResourceError::Traversal);
    }
    Ok(canon_candidate)
}

/// Windows 保留设备名分量（大小写不敏感，含带扩展名形式 `CON.txt`）。
/// 与 archive.rs 的 zip 条目校验同口径——服务路径同样不允许。
fn is_reserved_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("");
    let upper = stem.to_ascii_uppercase();
    let base = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL");
    let com = (1..=9).any(|i| upper == format!("COM{i}"));
    let lpt = (1..=9).any(|i| upper == format!("LPT{i}"));
    base || com || lpt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from("D:/pkg/extra.demo")
    }

    #[test]
    fn resolves_relative_paths_with_both_separators() {
        assert_eq!(
            resolve_page_resource(&root(), "index.html").unwrap(),
            root().join("index.html")
        );
        assert_eq!(
            resolve_page_resource(&root(), "web/index.html").unwrap(),
            root().join("web").join("index.html")
        );
        assert_eq!(
            resolve_page_resource(&root(), "web\\css\\page.css").unwrap(),
            root().join("web").join("css").join("page.css")
        );
        // ./ 前缀与重复分隔符容忍
        assert_eq!(
            resolve_page_resource(&root(), "./web//x.js").unwrap(),
            root().join("web").join("x.js")
        );
    }

    #[test]
    fn rejects_traversal_absolute_and_empty() {
        assert_eq!(
            resolve_page_resource(&root(), ""),
            Err(PageResourceError::Empty)
        );
        assert_eq!(
            resolve_page_resource(&root(), "   "),
            Err(PageResourceError::Empty)
        );
        assert_eq!(
            resolve_page_resource(&root(), "/etc/passwd"),
            Err(PageResourceError::Absolute)
        );
        assert_eq!(
            resolve_page_resource(&root(), "\\web\\x"),
            Err(PageResourceError::Absolute)
        );
        assert_eq!(
            resolve_page_resource(&root(), "C:/web/x"),
            Err(PageResourceError::Absolute)
        );
        assert_eq!(
            resolve_page_resource(&root(), "web/../../x"),
            Err(PageResourceError::Traversal)
        );
        assert_eq!(
            resolve_page_resource(&root(), "..\\x"),
            Err(PageResourceError::Traversal)
        );
    }

    #[test]
    fn rejects_reserved_device_segments() {
        assert_eq!(
            resolve_page_resource(&root(), "CON.html"),
            Err(PageResourceError::ReservedDevice)
        );
        assert_eq!(
            resolve_page_resource(&root(), "web/nul.js"),
            Err(PageResourceError::ReservedDevice)
        );
        assert_eq!(
            resolve_page_resource(&root(), "com1.txt"),
            Err(PageResourceError::ReservedDevice)
        );
        // 常规名字不受影响
        assert!(resolve_page_resource(&root(), "contact.html").is_ok());
        assert!(resolve_page_resource(&root(), "console.js").is_ok());
    }

    #[test]
    fn validate_entry_checks_existence_and_containment() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(pkg.join("web")).unwrap();
        std::fs::write(pkg.join("web").join("index.html"), "<html></html>").unwrap();

        assert!(
            validate_entry(&pkg, "web/index.html")
                .unwrap()
                .ends_with("index.html")
        );
        // 缺失文件 → Empty（调用方表现为页面缺失）
        assert_eq!(
            validate_entry(&pkg, "web/missing.html"),
            Err(PageResourceError::Empty)
        );
        // 目录不是入口
        assert_eq!(
            validate_entry(&pkg, "web"),
            Err(PageResourceError::Empty)
        );
    }
}
