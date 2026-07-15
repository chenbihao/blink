//! 密钥存储（0.9.1 Phase 2）——AI Provider API Key 唯一可信持久层。
//!
//! **架构**（对齐 phases/0.9-ai-layer.md §5）：
//!
//! - **持久层**：Windows Credential Manager（`CredWriteW` / `CRED_TYPE_GENERIC`），
//!   DPAPI 加密、账户级隔离，免自维护密钥派生
//! - **内存层**：`SecretString` newtype 包 `Zeroizing<String>`，drop 时按字节清零
//! - **SQLite**：**绝不**存 raw Key，只存 `secret_ref` 别名（如 `"blink/openai/key1"`）
//! - **tracing/log**：`Debug` impl 输出 `"<redacted>"`，`Display` 输出掩码 `••••{last4}`
//! - **前端**：只在"输入 → save_ai_secret invoke → 写 CM → 内存清零"这一次窗口里持有明文
//!
//! **五条铁则**（§5.1）：
//! 1. SQLite 只存 secret_ref，不存 raw Key
//! 2. 编辑 Key = 清空重填，禁止"保留旧 Key + 只改元数据"
//! 3. 删除 Provider = 立即 `CredDeleteW`，不作"标记删除"
//! 4. tracing/log/Debug 三通路都不能出现原文
//! 5. serde 序列化 Provider 类型必须 `#[serde(skip)]` secret 字段
//!
//! **纯逻辑抽出**：`build_target_name` / `format_masked` 是纯函数，跨平台单测覆盖；
//! 平台相关 CM 调用走 `windows.rs`（尚未落地平台后端时,mod 层仍能编译/测试）。

use std::fmt;
use zeroize::Zeroizing;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
#[allow(unused_imports)] // 0.9.1 Phase 2 定义,Phase 5 AI Provider dispatch 起消费
pub use windows::{delete_secret, load_secret, save_secret};

/// 内存中的密钥容器。**唯一**允许持有明文 Key 的类型。
///
/// - `Debug` 输出 `"SecretString(<redacted>)"`——不小心 `tracing::debug!(?secret)` 也不会泄漏
/// - `Display` 输出 `••••••{last4}`——设置页展示专用（`last4` 少于 4 字节则全 mask）
/// - `Drop` 走 `Zeroizing<String>` 的 drop → 内存字节清零，防 core dump / 内存快照泄漏
/// - **不实现 `Clone`**（有意为之）：想复制必须显式 `SecretString::new(s.expose().to_string())`,
///   强制开发者意识到"在增加明文副本"
/// - **不实现 `Serialize / Deserialize`**：绝不允许经 serde 走 IPC / 落盘
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    /// 从明文字串构造。**唯一构造入口**——只应在"从 CM 读出"或"从前端 invoke 参数拿到"两处调用。
    #[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 5 起 AIProvider dispatch 时消费
    pub fn new(raw: impl Into<String>) -> Self {
        Self(Zeroizing::new(raw.into()))
    }

    /// 暴露明文——**唯一破口**。命名故意刺眼,用它必须明确知道后果。
    ///
    /// **允许的调用者**：
    /// - `secret::save_secret` 内部：把明文写进 `CredWriteW`
    /// - `AIProvider::complete` 内部：把明文塞进 HTTP `Authorization` header
    /// - 测试代码：断言明文正确性
    ///
    /// **禁止的调用者**：
    /// - 任何 `tracing::*` / `println!` / `format!` 目标
    /// - 任何 serde 序列化 / 落盘 / IPC 路径
    #[allow(dead_code)]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 明文字节数。用于设置页展示"已配置"而不泄漏内容。
    #[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 5 起 AIProvider dispatch 时消费
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否空串——用户误提交空 Key 时快速判定。
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// `Debug` 绝不输出原文——即使 `tracing::debug!(?secret)` 也只会看到 "<redacted>"
impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

// `Display` 输出掩码——设置页展示"已配置"专用
impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_masked(&self.0))
    }
}

// ── 错误类型 ────────────────────────────────────────────────────────────────

/// 密钥操作错误。**故意不带原文密钥内容或平台 code**——上抛到 Result 也不能泄漏。
#[derive(Debug)]
#[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 5 起 AIProvider dispatch 时消费
pub enum SecretError {
    /// 平台调用失败(CM 写/读/删)。带一句人类可读描述,不含密钥字节。
    Platform(String),

    /// 别名不存在(读/删时命中)。
    NotFound(String),

    /// 别名非法(空 / 太长 / 含控制字符等)。
    InvalidRef(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(msg) => write!(f, "平台密钥操作失败: {msg}"),
            Self::NotFound(target) => write!(f, "密钥别名不存在: {target}"),
            Self::InvalidRef(target) => write!(f, "密钥别名非法: {target}"),
        }
    }
}

impl std::error::Error for SecretError {}

// ── 纯逻辑（跨平台可单测） ────────────────────────────────────────────────────

/// 命名空间前缀——所有 blink 存的密钥别名都是 `"blink/{provider_id}/{purpose}"`。
///
/// 这样即使用户装了别的应用往 Credential Manager 塞了同名条目,也不会互相覆盖;
/// 卸载脚本按此前缀批量清理无遗漏。
#[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 5 消费
pub const REF_NAMESPACE: &str = "blink";

/// 构造 CM target name(存进 `CREDENTIALW.TargetName`)。
///
/// - `provider_id`:UUID 或用户可读 ID(必须非空 + 不含 `/`,否则 IPC 反序列化风险)
/// - `purpose`:通常是 `"key"`,预留 `"secondary_key"` 等扩展位
///
/// 返回值形如 `"blink/1a2b3c/key"`。
#[allow(dead_code)]
pub fn build_target_name(provider_id: &str, purpose: &str) -> Result<String, SecretError> {
    if provider_id.is_empty() || provider_id.contains('/') || provider_id.contains('\0') {
        return Err(SecretError::InvalidRef(format!(
            "provider_id 非法: {provider_id:?}"
        )));
    }
    if purpose.is_empty() || purpose.contains('/') || purpose.contains('\0') {
        return Err(SecretError::InvalidRef(format!(
            "purpose 非法: {purpose:?}"
        )));
    }
    Ok(format!("{REF_NAMESPACE}/{provider_id}/{purpose}"))
}

/// 生成掩码字符串——UI 展示专用。
///
/// - 长度 ≤ 4:全 mask(避免暴露短 Key 的一半)
/// - 长度 > 4:前面固定 8 个 `•`,后面拼原文最后 4 个字节
///
/// 固定 8 个占位符是有意的:如果用真实长度做 mask,长/短 Key 一眼分辨——
/// 也是弱信息泄漏。设置页 UI 只需知道"已配置"。
#[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 6 前端设置页消费
pub fn format_masked(s: &str) -> String {
    if s.chars().count() <= 4 {
        return "••••".to_string();
    }
    let last4: String = s
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("••••••••{last4}")
}

/// 生成提示字符串——编辑 modal placeholder 专用,展示首尾各 4 字符。
///
/// - 长度 ≤ 8:退化为全掩码(太短则首尾重叠无意义)
/// - 长度 > 8:`{first4}••••{last4}` 形如 `sk-a••••cdef`
#[allow(dead_code)]
pub fn format_hint(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return "••••••••".to_string();
    }
    let first4: String = chars.iter().take(4).collect();
    let last4: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{first4}••••{last4}")
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_debug_never_leaks() {
        let s = SecretString::new("sk-1234567890abcdef".to_string());
        let dbg = format!("{s:?}");
        assert!(dbg.contains("<redacted>"), "Debug 输出必须掩码");
        assert!(!dbg.contains("sk-"), "Debug 输出不能含密钥前缀");
        assert!(!dbg.contains("1234567890"), "Debug 输出不能含密钥体");
    }

    #[test]
    fn secret_string_display_masks_correctly() {
        let s = SecretString::new("sk-1234567890abcdef".to_string());
        let disp = s.to_string();
        assert!(disp.starts_with("••••••••"), "Display 必须前缀 8 个 •");
        assert!(disp.ends_with("cdef"), "Display 必须后缀最后 4 字符");
        assert!(!disp.contains("sk-"));
        assert!(!disp.contains("12345"));
    }

    #[test]
    fn secret_string_short_key_fully_masked() {
        // ≤4 字节全掩码,防止"暴露一半"
        let s = SecretString::new("abc".to_string());
        assert_eq!(s.to_string(), "••••");
        let s = SecretString::new("abcd".to_string());
        assert_eq!(s.to_string(), "••••");
    }

    #[test]
    fn secret_string_expose_returns_raw() {
        let s = SecretString::new("sk-abc".to_string());
        assert_eq!(s.expose(), "sk-abc");
        assert_eq!(s.len(), 6);
        assert!(!s.is_empty());
    }

    #[test]
    fn secret_string_is_empty_true_for_empty() {
        let s = SecretString::new("".to_string());
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn build_target_name_happy_path() {
        assert_eq!(
            build_target_name("abc123", "key").unwrap(),
            "blink/abc123/key"
        );
    }

    #[test]
    fn build_target_name_rejects_empty_or_slash() {
        assert!(matches!(
            build_target_name("", "key").unwrap_err(),
            SecretError::InvalidRef(_)
        ));
        assert!(matches!(
            build_target_name("a/b", "key").unwrap_err(),
            SecretError::InvalidRef(_)
        ));
        assert!(matches!(
            build_target_name("abc", "").unwrap_err(),
            SecretError::InvalidRef(_)
        ));
        assert!(matches!(
            build_target_name("abc", "k/e").unwrap_err(),
            SecretError::InvalidRef(_)
        ));
    }

    #[test]
    fn build_target_name_rejects_null_byte() {
        // Windows CredWriteW 用 wide string,内部 \0 会截断——必须挡在前面
        assert!(matches!(
            build_target_name("a\0b", "key").unwrap_err(),
            SecretError::InvalidRef(_)
        ));
    }

    #[test]
    fn format_masked_unicode_boundary_safe() {
        // 中文 Key 极少见但要防"按字节切"导致 panic
        let s = SecretString::new("秘密密钥测试abc123".to_string());
        let disp = s.to_string();
        assert!(disp.ends_with("c123"));
        assert!(disp.starts_with("••••••••"));
    }

    #[test]
    fn format_hint_shows_first4_and_last4() {
        assert_eq!(format_hint("sk-1234567890abcdef"), "sk-1••••cdef");
        assert_eq!(format_hint("abcdefghij"), "abcd••••ghij");
    }

    #[test]
    fn format_hint_short_key_fully_masked() {
        assert_eq!(format_hint("abcdefgh"), "••••••••");
        assert_eq!(format_hint("short"), "••••••••");
    }

    #[test]
    fn format_hint_unicode_boundary_safe() {
        // 12 个字符 > 8,前后各 4
        assert_eq!(format_hint("秘密密钥测试数据值1234"), "秘密密钥••••1234");
    }
}
