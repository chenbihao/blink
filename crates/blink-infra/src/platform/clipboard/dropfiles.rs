//! CF_HDROP（DROPFILES）编解码与路径载荷边界（0.26.9）。
//!
//! 纯函数模块：Win32 侧只做内存搬运，编码、边界校验与去重键全部在此收敛，
//! 不依赖 `cfg(windows)` 即可单测。容量与时间值为本版初始参数（集中定义，
//! 可按实测调整并同步验收，不提升为跨版本铁则）。
//!
//! **边界语义**（0.26.9 §5.10）：任一上限超限整条拒绝——不保存截断后
//! 看似完整的文件列表；排序只用于去重比较，存储/写回/路径复制保留
//! 最近一次采集的原始顺序。

use sha2::{Digest, Sha256};

/// 单次快照允许的路径数上限。
pub const MAX_PATHS: usize = 256;
/// 单路径 UTF-16 code unit 上限（对齐 Windows 路径字符语义上限）。
pub const MAX_PATH_UTF16_UNITS: usize = 32_767;
/// 单条快照读取缓冲的 UTF-16 code unit 合计上限（1MiB）。
pub const MAX_SNAPSHOT_UTF16_UNITS: usize = 1024 * 1024;
/// 单条落库序列化载荷字节上限（1MiB）。
pub const MAX_SERIALIZED_BYTES: usize = 1024 * 1024;
/// `clipboard_files` 表序列化载荷合计上限（32MiB，容量淘汰依据）。
pub const MAX_TOTAL_PAYLOAD_BYTES: i64 = 32 * 1024 * 1024;

/// DROPFILES 结构大小（字节）：pFiles(4) + POINT(8) + fNC(4) + fWide(4)。
const DROPFILES_BYTES: usize = 20;

/// 路径拒绝原因（日志与测试用；日志只记原因与数量，不记路径原文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathReject {
    /// 空路径。
    Empty,
    /// 内嵌 NUL——编码进 DROPFILES 会截断列表。
    EmbeddedNul,
    /// 单路径超 UTF-16 code unit 上限。
    TooLong,
    /// 非绝对文件系统路径（相对路径 / 裸卷名等）。
    NotAbsolute,
    /// 设备命名空间路径（`\\.\` 或 `\\?\` 后非盘符/UNC 目标）。
    DeviceNamespace,
    /// 路径数超上限。
    TooMany,
    /// 读取缓冲或序列化载荷合计超上限。
    PayloadTooLarge,
}

impl PathReject {
    /// 结构化日志用的稳定原因码（不含路径内容）。
    pub fn reason(&self) -> &'static str {
        match self {
            PathReject::Empty => "empty",
            PathReject::EmbeddedNul => "embedded_nul",
            PathReject::TooLong => "too_long",
            PathReject::NotAbsolute => "not_absolute",
            PathReject::DeviceNamespace => "device_namespace",
            PathReject::TooMany => "too_many",
            PathReject::PayloadTooLarge => "payload_too_large",
        }
    }
}

/// 是否为绝对文件系统路径：盘符（`X:\` / `X:/`）、UNC（`\\server\share`）、
/// 长路径前缀（`\\?\C:\...` / `\\?\UNC\server\share`）。
///
/// 只看形态，不查文件系统；正斜杠盘符形式（`C:/foo`）也接受。
pub fn is_absolute_fs_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    // 盘符绝对路径：X:\ 或 X:/
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    // UNC：\\server\share（server 至少 1 字符且后跟分隔符）；
    // \\?\ 与 \\.\ 前缀交给下方长路径/设备命名空间分支
    if bytes.starts_with(b"\\\\") && bytes.len() > 3 {
        let rest = &bytes[2..];
        if !rest.starts_with(b"?\\") && !rest.starts_with(b".\\") {
            return rest.iter().any(|&b| b == b'\\' || b == b'/');
        }
    }
    // 长路径前缀：\\?\C:\... 或 \\?\UNC\server\share
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let rb = rest.as_bytes();
        if rb.len() >= 3
            && rb[0].is_ascii_alphabetic()
            && rb[1] == b':'
            && (rb[2] == b'\\' || rb[2] == b'/')
        {
            return true;
        }
        return rest.starts_with("UNC\\") && rest.len() > "UNC\\".len();
    }
    false
}

/// 是否为设备命名空间路径：`\\.\` 前缀，或 `\\?\` 后既非盘符也非 `UNC\`
/// （`\\?\Volume{...}` / `\\?\PhysicalDrive0` 等）。
///
/// 采集侧拒绝；读取侧（预览/入库）再次防御。
pub fn is_device_namespace(path: &str) -> bool {
    if path.starts_with(r"\\.\") {
        return true;
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let rb = rest.as_bytes();
        let is_drive = rb.len() >= 3
            && rb[0].is_ascii_alphabetic()
            && rb[1] == b':'
            && (rb[2] == b'\\' || rb[2] == b'/');
        return !(is_drive || rest.starts_with("UNC\\"));
    }
    false
}

/// 校验单条路径形态（非空、无内嵌 NUL、长度上限、非设备命名空间、绝对路径）。
///
/// 设备命名空间先于绝对路径判定——`\\.\` 前缀本就不是文件系统路径，
/// 先报 `DeviceNamespace` 更精确。
pub fn validate_single_path(path: &str) -> Result<(), PathReject> {
    if path.is_empty() {
        return Err(PathReject::Empty);
    }
    if path.contains('\u{0}') {
        return Err(PathReject::EmbeddedNul);
    }
    if path.encode_utf16().count() > MAX_PATH_UTF16_UNITS {
        return Err(PathReject::TooLong);
    }
    if is_device_namespace(path) {
        return Err(PathReject::DeviceNamespace);
    }
    if !is_absolute_fs_path(path) {
        return Err(PathReject::NotAbsolute);
    }
    Ok(())
}

/// 校验整组路径（数量 + 单条形态 + 读取缓冲合计上限）。
pub fn validate_paths(paths: &[String]) -> Result<(), PathReject> {
    if paths.is_empty() {
        return Err(PathReject::Empty);
    }
    if paths.len() > MAX_PATHS {
        return Err(PathReject::TooMany);
    }
    let mut total_units = 0usize;
    for path in paths {
        validate_single_path(path)?;
        total_units += path.encode_utf16().count();
        if total_units > MAX_SNAPSHOT_UTF16_UNITS {
            return Err(PathReject::PayloadTooLarge);
        }
    }
    Ok(())
}

/// 构造 `CF_HDROP` 载荷（DROPFILES 头 + UTF-16 路径 + 双 NUL 结束）。
///
/// 返回的 `Vec<u16>` 直接按字节写入 `HGLOBAL`；调用方需在编码前完成
/// `validate_paths`（本函数仍防御性拒绝内嵌 NUL，防截断列表）。
pub fn encode_dropfiles(paths: &[String]) -> Result<Vec<u16>, String> {
    for path in paths {
        if path.contains('\u{0}') {
            return Err("路径含内嵌 NUL，拒绝构造 DROPFILES".into());
        }
    }
    let mut units: Vec<u16> = Vec::with_capacity(DROPFILES_BYTES / 2 + 16);
    // pFiles = 20（文件列表偏移，字节）
    units.push(DROPFILES_BYTES as u16);
    units.push(0);
    // pt = (0, 0)（客户区坐标，粘贴方忽略）
    units.extend([0u16; 4]);
    // fNC = 0
    units.push(0);
    units.push(0);
    // fWide = 1（UTF-16）
    units.push(1);
    units.push(0);
    for path in paths {
        units.extend(path.encode_utf16());
        units.push(0);
    }
    // 列表结束的额外 NUL（双 NUL 终止）
    units.push(0);
    Ok(units)
}

/// 解析 `CF_HDROP` 载荷（仅 fWide=1）——编解码往返测试与防御性校验用。
///
/// 严格按 shell 语义：从 `pFiles` 偏移起按 NUL 分割，空串即列表结束。
/// 载荷过短、非宽字符、偏移越界返回 `None`。
pub fn parse_dropfiles(payload: &[u16]) -> Option<Vec<String>> {
    if payload.len() < DROPFILES_BYTES / 2 {
        return None;
    }
    let p_files = u32::from(payload[0]) | (u32::from(payload[1]) << 16);
    let f_wide = u32::from(payload[8]) | (u32::from(payload[9]) << 16);
    if f_wide != 1 {
        return None;
    }
    if p_files as usize % 2 != 0 || p_files as usize / 2 >= payload.len() {
        return None;
    }
    let mut paths = Vec::new();
    let mut cursor = p_files as usize / 2;
    while cursor < payload.len() {
        let end = payload[cursor..].iter().position(|&u| u == 0)? + cursor;
        if end == cursor {
            break; // 连续 NUL = 列表结束
        }
        paths.push(String::from_utf16(&payload[cursor..end]).ok()?);
        cursor = end + 1;
    }
    Some(paths)
}

/// 去重键：排序后路径列表的无歧义序列化（JSON）的 SHA-256。
///
/// 排序仅用于比较；哈希命中后仍需核对列表本身（防碰撞），核对以
/// 「存储 JSON → 解析 → 排序 → 序列化」与待存列表的排序序列化比对。
pub fn dedup_key(paths: &[String]) -> String {
    let mut sorted = paths.to_vec();
    sorted.sort();
    let canonical = serde_json::to_string(&sorted).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 原始顺序 JSON 序列化（落库载荷），超 1MiB 拒绝。
pub fn serialize_paths(paths: &[String]) -> Result<String, String> {
    let json = serde_json::to_string(paths).map_err(|e| e.to_string())?;
    if json.len() > MAX_SERIALIZED_BYTES {
        return Err(format!(
            "路径序列化载荷超上限: {} > {MAX_SERIALIZED_BYTES}",
            json.len()
        ));
    }
    Ok(json)
}

/// 路径的文件名部分（不含目录）；无文件名（如根目录）返回 `None`。
pub fn first_file_name(path: &str) -> Option<&str> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())?;
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_parse_roundtrip_multi_unicode() {
        let paths = vec![
            r"C:\Users\测试\图片 📷.png".to_string(),
            r"\\server\share\file.txt".to_string(),
            r"C:/正斜杠/路径.docx".to_string(),
        ];
        let payload = encode_dropfiles(&paths).unwrap();
        // 头 10 units + 双 NUL 结束
        assert_eq!(payload[0], 20);
        assert_eq!(payload[8], 1, "fWide=1");
        let parsed = parse_dropfiles(&payload).unwrap();
        assert_eq!(parsed, paths);
        // 末尾双 NUL：最后两个 unit 都是 0
        let n = payload.len();
        assert_eq!(payload[n - 1], 0);
        assert_eq!(payload[n - 2], 0);
    }

    #[test]
    fn encode_single_path_has_trailing_double_nul() {
        let payload = encode_dropfiles(&[r"C:\a.txt".to_string()]).unwrap();
        let parsed = parse_dropfiles(&payload).unwrap();
        assert_eq!(parsed, vec![r"C:\a.txt".to_string()]);
    }

    #[test]
    fn encode_rejects_embedded_nul() {
        assert!(encode_dropfiles(&["C:\\a\0b".to_string()]).is_err());
    }

    #[test]
    fn parse_rejects_malformed_payloads() {
        assert!(parse_dropfiles(&[]).is_none(), "过短");
        assert!(parse_dropfiles(&[0; 10]).is_none(), "pFiles 偏移越界");
        let mut ansi = encode_dropfiles(&[r"C:\a".to_string()]).unwrap();
        ansi[8] = 0; // fWide=0
        assert!(parse_dropfiles(&ansi).is_none(), "仅支持宽字符");
        // 偏移奇数字节
        let mut odd = encode_dropfiles(&[r"C:\a".to_string()]).unwrap();
        odd[0] = 21;
        assert!(parse_dropfiles(&odd).is_none());
    }

    #[test]
    fn long_path_within_bound_roundtrips() {
        let long = format!(r"C:\{}", "a".repeat(32_000));
        assert!(validate_single_path(&long).is_ok());
        let payload = encode_dropfiles(&[long.clone()]).unwrap();
        assert_eq!(parse_dropfiles(&payload).unwrap(), vec![long]);
        let over = format!(r"C:\{}", "a".repeat(MAX_PATH_UTF16_UNITS));
        assert_eq!(validate_single_path(&over), Err(PathReject::TooLong));
    }

    #[test]
    fn validate_paths_enforces_count_and_total_units() {
        let mut many = Vec::new();
        for i in 0..MAX_PATHS {
            many.push(format!(r"C:\f{i}"));
        }
        assert!(validate_paths(&many).is_ok());
        many.push(r"C:\extra".into());
        assert_eq!(validate_paths(&many), Err(PathReject::TooMany));

        // 合计 units 超 1MiB：256 条 × 5000 units
        let unit_path = format!(r"C:\{}", "b".repeat(4990));
        let big: Vec<String> = (0..256).map(|_| unit_path.clone()).collect();
        assert_eq!(validate_paths(&big), Err(PathReject::PayloadTooLarge));

        assert_eq!(validate_paths(&[]), Err(PathReject::Empty));
    }

    #[test]
    fn absolute_path_accepts_drive_unc_and_long_prefix() {
        assert!(is_absolute_fs_path(r"C:\a\b.txt"));
        assert!(is_absolute_fs_path(r"c:/a/b.txt"));
        assert!(is_absolute_fs_path(r"\\server\share\f.txt"));
        assert!(is_absolute_fs_path(r"\\?\C:\long\a.txt"));
        assert!(is_absolute_fs_path(r"\\?\UNC\server\share\a.txt"));
        assert!(!is_absolute_fs_path(r"relative\path.txt"));
        assert!(!is_absolute_fs_path(r"C:nonseparator"));
        assert!(!is_absolute_fs_path(""));
        assert!(!is_absolute_fs_path(r"\\"));
    }

    #[test]
    fn device_namespace_rejected_but_drive_and_unc_allowed() {
        assert!(is_device_namespace(r"\\.\PhysicalDrive0"));
        assert!(is_device_namespace(r"\\?\Volume{12345678-0000}"));
        assert!(is_device_namespace(r"\\.\COM1"));
        assert!(!is_device_namespace(r"\\?\C:\a.txt"));
        assert!(!is_device_namespace(r"\\?\UNC\server\share\a.txt"));
        assert!(!is_device_namespace(r"C:\a.txt"));
        assert!(!is_device_namespace(r"\\server\share\a.txt"));
        // 采集校验连带拒绝设备命名空间
        assert_eq!(
            validate_single_path(r"\\.\PhysicalDrive0"),
            Err(PathReject::DeviceNamespace)
        );
    }

    #[test]
    fn dedup_key_order_insensitive_and_collision_distinct() {
        let a = vec![r"C:\a.txt".to_string(), r"C:\b.txt".to_string()];
        let b = vec![r"C:\b.txt".to_string(), r"C:\a.txt".to_string()];
        let c = vec![r"C:\a.txt".to_string(), r"C:\c.txt".to_string()];
        assert_eq!(dedup_key(&a), dedup_key(&b), "乱序同组合同键");
        assert_ne!(dedup_key(&a), dedup_key(&c));
        // 无分隔歧义：["C:\\a", "b.txt"] 与 ["C:\\a\", ...] 序列化不同
        let x = vec![r"C:\a".to_string(), r"b.txt".to_string()];
        let y = vec![r"C:\a\b.txt".to_string()];
        assert_ne!(dedup_key(&x), dedup_key(&y));
    }

    #[test]
    fn serialize_paths_enforces_byte_bound() {
        let ok = vec![r"C:\a.txt".to_string()];
        assert!(serialize_paths(&ok).is_ok());
        let big = vec![format!(r"C:\{}", "c".repeat(MAX_SERIALIZED_BYTES))];
        assert!(serialize_paths(&big).is_err());
    }

    #[test]
    fn first_file_name_extracts_name() {
        assert_eq!(first_file_name(r"C:\dir\a.txt"), Some("a.txt"));
        assert_eq!(first_file_name(r"C:\a.txt"), Some("a.txt"));
        assert_eq!(first_file_name(r"\\server\share\f.bin"), Some("f.bin"));
        assert_eq!(first_file_name(r"C:\"), None);
    }
}
