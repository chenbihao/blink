//! 归档安全解压（0.28.1 §五.3）——zip 展开的越界/链接/总量防线。
//!
//! 威胁模型：恶意或损坏的插件归档。拒绝路径穿越（`..`/绝对路径/盘符）、
//! 符号链接条目、加密条目；限制单文件与总解压量、条目数（zip bomb）。
//! 业务校验（manifest/兼容性/命名空间）在 domain/plugin/install.rs。

use std::io::Read;
use std::path::Path;

/// 解压限额。默认值按官方插件包量级（词库大包 0.28.4 走独立数据包通道，
/// 不塞进插件 zip）。
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    /// 单文件解压上限（字节）。
    pub max_single_file: u64,
    /// 总解压量上限（字节）。
    pub max_total: u64,
    /// 条目数上限。
    pub max_entries: usize,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        ExtractLimits {
            max_single_file: 128 * 1024 * 1024,
            max_total: 512 * 1024 * 1024,
            max_entries: 5000,
        }
    }
}

/// 解压结果摘要。
#[derive(Debug, Clone)]
pub struct ExtractReport {
    pub entry_count: usize,
    pub total_bytes: u64,
    /// 全部条目共享的唯一顶层目录名（常见 `pkg-root/...` 包装层）；
    /// 多顶层或裸文件时为 None（由调用方决定是否接受）。
    pub single_root_dir: Option<String>,
}

/// 归档错误（越界/链接/超量/损坏——全部拒绝整个包，不部分落地）。
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("归档读取失败: {0}")]
    Open(String),
    #[error("条目越界或非法路径: {0}")]
    UnsafePath(String),
    #[error("符号链接条目被拒绝: {0}")]
    Symlink(String),
    #[error("加密条目被拒绝: {0}")]
    Encrypted(String),
    #[error("超出限额: {0}")]
    Limit(String),
    #[error("解压 IO 失败: {0}")]
    Io(String),
}

/// 校验 zip 内条目名可安全落到 `dest` 下，返回归一化（`/` 分隔）路径。
///
/// 拒绝：绝对路径（`/x`、`\x`）、盘符（`C:`）、任何 `..` 段、
/// Windows 保留设备名（CON/NUL/COM1…，防老 API 路径逃逸）。
/// 不走 `Path` 组件遍历——Windows 上会改写分隔符，破坏后续 split('/')。
pub fn validate_entry_name(raw: &str) -> Result<String, ArchiveError> {
    // 统一按 zip 规范的 '/' 分隔检查；反斜杠视为非法（正规工具不发）
    if raw.contains('\\') {
        return Err(ArchiveError::UnsafePath(raw.to_string()));
    }
    if raw.starts_with('/') {
        return Err(ArchiveError::UnsafePath(raw.to_string()));
    }
    let mut clean = String::with_capacity(raw.len());
    for seg in raw.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(ArchiveError::UnsafePath(raw.to_string()));
        }
        if seg.contains(':') {
            return Err(ArchiveError::UnsafePath(raw.to_string()));
        }
        if is_windows_device_name(seg) {
            return Err(ArchiveError::UnsafePath(raw.to_string()));
        }
        if !clean.is_empty() {
            clean.push('/');
        }
        clean.push_str(seg);
    }
    if clean.is_empty() {
        return Err(ArchiveError::UnsafePath(raw.to_string()));
    }
    Ok(clean)
}

fn is_windows_device_name(seg: &str) -> bool {
    let upper = seg.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6"
            | "COM7" | "COM8" | "COM9" | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6"
            | "LPT7" | "LPT8" | "LPT9"
    )
}

/// zip 条目的 Unix 符号链接判定（external_attributes 高 16 位 mode，
/// S_IFLNK = 0o120000）。
fn is_symlink_entry(entry: &zip::read::ZipFile<'_>) -> bool {
    let attr = entry.unix_mode().unwrap_or(0);
    attr & 0o170000 == 0o120000
}

/// 安全解压 zip 到 `dest`（dest 须为空或不存在）。
///
/// 返回摘要（含 single_root_dir 侦察）。任何条目非法即返回 Err——
/// 之前已落地的部分留在 dest，由调用方整体丢弃（staging 语义）。
pub fn extract_zip_safely(
    zip_path: &Path,
    dest: &Path,
    limits: ExtractLimits,
) -> Result<ExtractReport, ArchiveError> {
    let file = std::fs::File::open(zip_path).map_err(|e| ArchiveError::Open(e.to_string()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| ArchiveError::Open(e.to_string()))?;

    if archive.len() as usize > limits.max_entries {
        return Err(ArchiveError::Limit(format!(
            "条目数 {} 超上限 {}",
            archive.len(),
            limits.max_entries
        )));
    }
    std::fs::create_dir_all(dest).map_err(|e| ArchiveError::Io(e.to_string()))?;

    let mut total: u64 = 0;
    let mut top_dirs: Vec<String> = Vec::new();
    for idx in 0..archive.len() {
        // by_index（非 raw）：read 走解压流，written 才能对得上声明的未压缩大小。
        let mut entry = archive
            .by_index(idx)
            .map_err(|e| ArchiveError::Open(e.to_string()))?;
        let raw_name = entry.name().to_string();
        if is_symlink_entry(&entry) {
            return Err(ArchiveError::Symlink(raw_name));
        }
        if entry.encrypted() {
            return Err(ArchiveError::Encrypted(raw_name));
        }
        let safe = validate_entry_name(&raw_name)?;
        let out_path = dest.join(&safe);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| ArchiveError::Io(e.to_string()))?;
            continue;
        }
        // 记录顶层段（用于 single_root_dir 侦察）
        if let Some(top) = safe.split('/').next()
            && !top.is_empty()
            && !top_dirs.iter().any(|d| d == top)
        {
            top_dirs.push(top.to_string());
        }

        let declared = entry.size();
        if declared > limits.max_single_file {
            return Err(ArchiveError::Limit(format!(
                "条目 {safe} 声明大小 {declared} 超单文件上限"
            )));
        }
        if total.saturating_add(declared) > limits.max_total {
            return Err(ArchiveError::Limit("总解压量超上限".into()));
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ArchiveError::Io(e.to_string()))?;
        }
        let mut out = std::fs::File::create(&out_path).map_err(|e| ArchiveError::Io(e.to_string()))?;
        let mut written: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = entry.read(&mut buf).map_err(|e| ArchiveError::Io(e.to_string()))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            // 防声明大小造假的硬上限：实际写出超限即停
            if written > limits.max_single_file || total + written > limits.max_total {
                return Err(ArchiveError::Limit(format!("条目 {safe} 实际写出超限")));
            }
            std::io::Write::write_all(&mut out, &buf[..n])
                .map_err(|e| ArchiveError::Io(e.to_string()))?;
        }
        if written != declared {
            return Err(ArchiveError::Io(format!(
                "条目 {safe} 大小不符（声明 {declared} 实写 {written}，归档损坏）"
            )));
        }
        total += written;
    }

    Ok(ExtractReport {
        entry_count: archive.len() as usize,
        total_bytes: total,
        single_root_dir: (top_dirs.len() == 1).then(|| top_dirs[0].clone()),
    })
}

/// 复制目录树（dev 模式从解包目录安装用），应用与 zip 相同的限额。
///
/// 拒绝符号链接与 junction（`symlink_metadata` file_type 判定）。
/// src 直接作为包根（无 zip 那种顶层包装层侦察）。
pub fn copy_dir_bounded(
    src: &Path,
    dest: &Path,
    limits: ExtractLimits,
) -> Result<ExtractReport, ArchiveError> {
    std::fs::create_dir_all(dest).map_err(|e| ArchiveError::Io(e.to_string()))?;
    let mut state = (0usize, 0u64);
    walk_copy(src, dest, limits, &mut state)?;
    Ok(ExtractReport {
        entry_count: state.0,
        total_bytes: state.1,
        single_root_dir: None,
    })
}

fn walk_copy(
    src: &Path,
    dest: &Path,
    limits: ExtractLimits,
    state: &mut (usize, u64),
) -> Result<(), ArchiveError> {
    let entries = std::fs::read_dir(src).map_err(|e| ArchiveError::Io(e.to_string()))?;
    for entry in entries.flatten() {
        let ft = entry.file_type().map_err(|e| ArchiveError::Io(e.to_string()))?;
        if ft.is_symlink() {
            return Err(ArchiveError::Symlink(entry.path().to_string_lossy().into_owned()));
        }
        let out = dest.join(entry.file_name());
        if ft.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| ArchiveError::Io(e.to_string()))?;
            walk_copy(&entry.path(), &out, limits, state)?;
        } else {
            state.0 += 1;
            if state.0 > limits.max_entries {
                return Err(ArchiveError::Limit("条目数超上限".into()));
            }
            let meta = entry.metadata().map_err(|e| ArchiveError::Io(e.to_string()))?;
            if meta.len() > limits.max_single_file {
                return Err(ArchiveError::Limit("单文件超上限".into()));
            }
            state.1 += meta.len();
            if state.1 > limits.max_total {
                return Err(ArchiveError::Limit("总复制量超上限".into()));
            }
            std::fs::copy(entry.path(), &out).map_err(|e| ArchiveError::Io(e.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zip(path: &Path, entries: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        for (name, content) in entries {
            zip.start_file(*name, options).unwrap();
            std::io::Write::write_all(&mut zip, content.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn normal_zip_extracts_and_reports_single_root() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("pkg.zip");
        make_zip(
            &zip_path,
            &[
                ("pkg/manifest.json", "{}"),
                ("pkg/bin/tool.exe", "MZ"),
                ("pkg/assets/style.css", "body{}"),
            ],
        );
        let dest = tmp.path().join("out");
        let report = extract_zip_safely(&zip_path, &dest, ExtractLimits::default()).unwrap();
        assert_eq!(report.single_root_dir.as_deref(), Some("pkg"));
        assert_eq!(report.entry_count, 3);
        assert!(dest.join("pkg/bin/tool.exe").is_file());
    }

    #[test]
    fn traversal_and_absolute_paths_rejected() {
        for bad in ["../evil.txt", "a/../../evil.txt", "/abs.txt", "C:/evil.txt", "a\\b.txt"] {
            assert!(validate_entry_name(bad).is_err(), "{bad} 应被拒绝");
        }
        assert!(validate_entry_name("pkg/bin/tool.exe").is_ok());
        assert!(validate_entry_name("./pkg/x").is_ok());
    }

    /// ZipWriter 会自行净化 `../` 路径，穿越包造不出来——穿越防线由
    /// `validate_entry_name` 单测覆盖；这里覆盖 zip 层的符号链接拒绝
    /// （`add_symlink` 产生 S_IFLNK mode 条目，`is_symlink_entry` 捕获）。
    #[test]
    fn zip_with_symlink_entry_rejected_wholesale() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("evil.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
            .unix_permissions(0o120000);
        zip.add_symlink("link.txt", "target.txt", options).unwrap();
        let plain: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        zip.start_file("good.txt", plain).unwrap();
        std::io::Write::write_all(&mut zip, b"ok").unwrap();
        zip.finish().unwrap();
        let dest = tmp.path().join("out");
        assert!(matches!(
            extract_zip_safely(&zip_path, &dest, ExtractLimits::default()),
            Err(ArchiveError::Symlink(_))
        ));
    }

    #[test]
    fn limits_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("big.zip");
        make_zip(&zip_path, &[("a.bin", &"x".repeat(10_000))]);
        let dest = tmp.path().join("out");
        let tight = ExtractLimits {
            max_single_file: 5_000,
            ..ExtractLimits::default()
        };
        assert!(matches!(
            extract_zip_safely(&zip_path, &dest, tight),
            Err(ArchiveError::Limit(_))
        ));
        // 条目数上限
        let many = tmp.path().join("many.zip");
        make_zip(&many, &[("a", "1"), ("b", "2")]);
        let tight_entries = ExtractLimits {
            max_entries: 1,
            ..ExtractLimits::default()
        };
        assert!(matches!(
            extract_zip_safely(&many, &tmp.path().join("out2"), tight_entries),
            Err(ArchiveError::Limit(_))
        ));
    }

    #[test]
    fn copy_dir_bounded_rejects_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/f.txt"), "hi").unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("f.txt", src.join("sub/link.txt")).unwrap();
        let dest = tmp.path().join("dest");
        let res = copy_dir_bounded(&src, &dest, ExtractLimits::default());
        if cfg!(windows) {
            assert!(matches!(res, Err(ArchiveError::Symlink(_))));
        } else {
            res.unwrap();
        }
    }
}
