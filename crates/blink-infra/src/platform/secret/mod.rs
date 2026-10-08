//! 密钥存储（0.9.1 Phase 2 → 0.17.11 keyring 重构 → 0.25.15 v2 命名）——AI Provider /
//! STT / 插件密钥唯一可信持久层。
//!
//! **架构**（0.25.15 起）：
//!
//! - **持久层**：`keyring` crate（v1 API 初始化平台默认 store）+
//!   `keyring_core::Entry::new_with_modifiers` 的 `target` modifier——显式指定完整
//!   CM target name，绕开 keyring 默认 `{user}.{service}` 拼接（应用名在尾部的反序形状）
//! - **命名**：`build_target_name_v2(SecretRef)` 产出 `blink:{module}:{subject}:{purpose}`
//!   （如 `blink:ai:{uuid}:key` / `blink:stt:cloud:key`），应用前缀置首
//! - **内存层**：`SecretString` newtype 包 `Zeroizing<String>`，drop 时按字节清零
//! - **SQLite**：**绝不**存 raw Key，只存 `secret_ref` 别名
//! - **tracing/log**：`Debug` impl 输出 `"<redacted>"`，`Display` 输出掩码 `••••{last4}`
//! - **前端**：只在"输入 → save invoke → 写 keyring → 内存清零"这一次窗口里持有明文
//!
//! **命名演进**（详见 `build_target_name_v2`）：
//! - v0 `blink/{pid}/{purpose}`（自写 FFI，0.17.11 前）
//! - v1 `{pid}/{purpose}.blink`（keyring v1 拼接，0.17.11–0.25.14）
//! - v2 `blink:{module}:{subject}:{purpose}`（0.25.15 起）
//!
//! **0.17.11 keyring 重构**：
//! - `store.rs` — keyring 后端，提供 `save_secret`/`load_secret`/`delete_secret`（跨平台）
//! - `windows_legacy.rs` — 保留 `CredEnumerateW` 枚举 + raw 读删（迁移 + 诊断用）
//! - `migrate.rs` — 启动期一次性迁移 v0 老 CM `blink/*` 条目到新命名
//! - `migrate_v2.rs` — 启动期自愈收编 v1 `{pid}/key.blink` 条目到 v2 命名（0.25.15；
//!   0.25.16-fix 去 marker——每次启动全量枚举扫描，无残留即 no-op）
//! - 单测用自写内存 store（支持 target modifier），绝不碰真实 CM
//!   （根除 `cargo test` 清空生产 CM 的元凶）
//!
//! **五条铁则**（§5.1）：
//! 1. SQLite 只存 secret_ref，不存 raw Key
//! 2. 编辑 Key = 清空重填，禁止"保留旧 Key + 只改元数据"
//! 3. 删除 Provider = 立即删密钥，不作"标记删除"
//! 4. tracing/log/Debug 三通路都不能出现原文
//! 5. serde 序列化 Provider 类型必须 `#[serde(skip)]` secret 字段
//!
//! **纯逻辑抽出**：`build_target_name` / `build_target_name_v2` / `format_masked`
//! 是纯函数，跨平台单测覆盖。

use std::fmt;
use zeroize::Zeroizing;

#[cfg(target_os = "windows")]
mod store; // keyring 后端（0.17.11 起，替换 windows.rs 的 unsafe FFI）

#[cfg(target_os = "windows")]
mod windows_legacy; // 原 windows.rs 改名，保留 enumerate + 迁移用 raw 读删

#[cfg(target_os = "windows")]
pub mod migrate; // v0 CM→keyring 一次性迁移（0.17.11）

#[cfg(target_os = "windows")]
pub mod migrate_v2; // keyring v1 命名→v2 命名启动期自愈收编（0.25.15；0.25.16-fix 无 marker）

// 生产密钥读写——从 store.rs re-export（0.25.15 签名改 SecretRef 入参）
#[cfg(target_os = "windows")]
pub use store::{delete_secret, load_secret, save_secret};

// 枚举老 CM 条目——从 windows_legacy.rs re-export（诊断 + 迁移用）
#[cfg(target_os = "windows")]
pub use windows_legacy::enumerate_all_secrets;

// 全量枚举 + Rust 端 contains 过滤——跨三代命名的迁移/计数用（0.25.16-fix）
#[cfg(target_os = "windows")]
pub fn enumerate_blink_secrets_anywhere() -> Result<Vec<SecretInfo>, SecretError> {
    Ok(windows_legacy::enumerate_all_secrets()?
        .into_iter()
        .filter(|s| s.target_name.contains(REF_NAMESPACE))
        .collect())
}

/// 批量删除全部密钥（0.17.11 改为遍历配置逐个删；0.25.15 入参改 `SecretRef` 列表）。
///
/// **0.17.11 前**：此函数用 `CredEnumerateW` 枚举 CM 中所有 `blink/*` 条目后逐个删。
/// **0.17.11 起**：改为接受密钥引用列表，逐个调 `store::delete_secret`。
/// 这样：
/// - 清理的是 keyring 新命名下的条目（与生产存储一致）
/// - 不会误删其他应用的 `blink/*` 条目（更可控）
/// - `cleanup_all_data` 调用时，调用方需在删数据目录前先读出 provider id 列表
///
/// `refs` 应包含所有需要清理的密钥引用——AI 各 provider（`SecretRef::ai`）+
/// STT 云端（`SecretRef::stt_cloud()`）+ 插件密钥字段（`SecretRef::plugin`，0.25.16）。
/// `NotFound` 视为成功（幂等——可能被其他途径已删）。
///
/// # 返回
/// `Vec<(target_name, Result<(), SecretError>)>`
#[cfg(target_os = "windows")]
pub fn delete_all_blink_secrets(refs: &[SecretRef]) -> Vec<(String, Result<(), SecretError>)> {
    let mut results = Vec::with_capacity(refs.len());
    for r in refs {
        let target = build_target_name_v2(r).unwrap_or_else(|_| r.subject.clone());
        match store::delete_secret(r) {
            Ok(()) => {
                tracing::debug!(target = %target, "批量删除密钥成功");
                results.push((target, Ok(())));
            }
            Err(SecretError::NotFound(_)) => {
                // NotFound 视为成功（幂等）
                results.push((target, Ok(())));
            }
            Err(e) => {
                tracing::warn!(target = %target, error = %e, "批量删除密钥失败");
                results.push((target, Err(e)));
            }
        }
    }
    tracing::info!(
        total = results.len(),
        failed = results.iter().filter(|(_, r)| r.is_err()).count(),
        "批量删除密钥完成（遍历配置）"
    );
    results
}

/// 密钥元信息——枚举 CM 中 `blink/*` 条目时返回。
///
/// **不含密钥内容**，只有 target name（如 `"blink/openai/key1"`），
/// 供设置页 / 卸载清理展示与确认。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecretInfo {
    /// CM TargetName，形如 `"blink/{provider_id}/{purpose}"`
    pub target_name: String,
}

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

/// 命名空间前缀——v2 命名 `blink:{module}:{subject}:{purpose}` 的第一段（0.25.15）。
///
/// 历史：v0 命名 `blink/{provider_id}/{purpose}`（自写 FFI 时代）；
/// v1 命名 `{provider_id}/{purpose}.blink`（keyring v1 API 的 `{user}.{service}` 拼接，
/// 应用名跑到尾部）；v2 起应用前缀置首，与常见应用习惯一致。
pub const REF_NAMESPACE: &str = "blink";

/// 密钥所属模块——CM target name 的命名空间段（0.25.15）。
///
/// 显式建模取代 0.17.11 前的"pid 字符串约定"（`stt:cloud` 这种魔法前缀），
/// 模块归属在类型层面钉死，写入/迁移/清理共用同一份语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretModule {
    /// AI Provider 密钥（subject = provider id：UUID 或历史 slug）
    Ai,
    /// STT 云端密钥（subject 固定 `"cloud"`，承接历史别名 `stt:cloud`）
    Stt,
    /// 插件 settings 密钥字段（subject = plugin_id，purpose = 字段 key，0.25.16）
    Plugin,
}

impl SecretModule {
    /// target name 中的段名。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ai => "ai",
            Self::Stt => "stt",
            Self::Plugin => "plugin",
        }
    }
}

/// 密钥引用——`(模块, 主体, 用途)` 三元组，唯一确定一条 CM 条目（0.25.15）。
///
/// 取代 0.25.15 前 `save_secret(provider_id, purpose)` 的裸字符串签名：
/// provider_id 曾身兼两职（AI 的 UUID / STT 的 `stt:cloud` 魔法别名），
/// 现在模块归属显式化，`build_target_name_v2` 据此产出应用前缀置首的 target。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub module: SecretModule,
    pub subject: String,
    pub purpose: String,
}

impl SecretRef {
    /// AI Provider 主密钥（purpose 固定 `"key"`，扩展位留给 secondary_key 等）。
    pub fn ai(provider_id: &str) -> Self {
        Self {
            module: SecretModule::Ai,
            subject: provider_id.to_string(),
            purpose: "key".to_string(),
        }
    }

    /// STT 云端密钥（承接历史固定别名 `stt:cloud`）。
    pub fn stt_cloud() -> Self {
        Self {
            module: SecretModule::Stt,
            subject: "cloud".to_string(),
            purpose: "key".to_string(),
        }
    }

    /// 插件 settings 密钥字段（0.25.16 前端/宿主写入拦截用）。
    pub fn plugin(plugin_id: &str, field_key: &str) -> Self {
        Self {
            module: SecretModule::Plugin,
            subject: plugin_id.to_string(),
            purpose: field_key.to_string(),
        }
    }

    /// 从 0.25.15 前的 pid 恢复命名空间（两代迁移共用）。
    ///
    /// 历史 pid 只有两类：`stt:cloud`（STT 固定别名）与其余（AI provider id——
    /// UUID 或 0.14.6–0.17.11 期间的显示名 slug，如 `sensenova`）。
    pub fn from_legacy_pid(pid: &str, purpose: &str) -> Self {
        if pid == "stt:cloud" {
            Self::stt_cloud()
        } else {
            Self {
                module: SecretModule::Ai,
                subject: pid.to_string(),
                purpose: purpose.to_string(),
            }
        }
    }
}

/// 构造 v2 CM target name：`blink:{module}:{subject}:{purpose}`（0.25.15）。
///
/// 纯函数——单测直接断言字符串形状。`subject`/`purpose` 禁止 `:` 与 `\0`
/// （保证 target 可按段解析回 SecretRef；AI UUID/slug、`cloud`、plugin_id、
/// settings 字段 key 均天然满足）。
pub fn build_target_name_v2(r: &SecretRef) -> Result<String, SecretError> {
    for (name, seg) in [("subject", &r.subject), ("purpose", &r.purpose)] {
        if seg.is_empty() || seg.contains(':') || seg.contains('\0') {
            return Err(SecretError::InvalidRef(format!(
                "SecretRef {name} 非法: {seg:?}"
            )));
        }
    }
    Ok(format!(
        "{REF_NAMESPACE}:{}:{}:{}",
        r.module.as_str(),
        r.subject,
        r.purpose
    ))
}

/// 构造 CM target name(存进 `CREDENTIALW.TargetName`)。
///
/// - `provider_id`:UUID 或用户可读 ID(必须非空 + 不含 `/`,否则 IPC 反序列化风险)
/// - `purpose`:通常是 `"key"`,预留 `"secondary_key"` 等扩展位
///
/// 返回值形如 `"blink/1a2b3c/key"`。
///
/// **0.25.15 起为 v0 legacy 命名**——仅 `windows_legacy.rs`（迁移 + raw 读删）使用，
/// 生产读写走 `build_target_name_v2`。
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

    // ── 0.25.15 v2 命名 ─────────────────────────────────────────────────────

    #[test]
    fn build_target_name_v2_shapes() {
        // AI：UUID provider
        assert_eq!(
            build_target_name_v2(&SecretRef::ai("28f6b24c-2e22-4abc-b919-76cf62a4644f")).unwrap(),
            "blink:ai:28f6b24c-2e22-4abc-b919-76cf62a4644f:key"
        );
        // AI：历史 slug provider（0.14.6–0.17.11 期间创建）
        assert_eq!(
            build_target_name_v2(&SecretRef::ai("sensenova")).unwrap(),
            "blink:ai:sensenova:key"
        );
        // STT
        assert_eq!(
            build_target_name_v2(&SecretRef::stt_cloud()).unwrap(),
            "blink:stt:cloud:key"
        );
        // 插件（0.25.16 消费；plugin_id 含点合法）
        assert_eq!(
            build_target_name_v2(&SecretRef::plugin("builtin.translate", "deepl_api_key")).unwrap(),
            "blink:plugin:builtin.translate:deepl_api_key"
        );
    }

    #[test]
    fn build_target_name_v2_rejects_bad_segments() {
        // subject/purpose 含 : 或为空 → InvalidRef（保证可按段解析）
        for r in [
            SecretRef::ai("a:b"),
            SecretRef::ai(""),
            SecretRef::plugin("p", "field:key"),
            SecretRef::plugin("p", ""),
            SecretRef::ai("a\0b"),
        ] {
            assert!(
                matches!(build_target_name_v2(&r), Err(SecretError::InvalidRef(_))),
                "应拒绝非法 SecretRef: {r:?}"
            );
        }
    }

    #[test]
    fn from_legacy_pid_maps_stt_and_ai() {
        assert_eq!(
            SecretRef::from_legacy_pid("stt:cloud", "key"),
            SecretRef::stt_cloud()
        );
        assert_eq!(
            SecretRef::from_legacy_pid("sensenova", "key"),
            SecretRef::ai("sensenova")
        );
        assert_eq!(
            SecretRef::from_legacy_pid("550e8400-e29b-41d4-a716-446655440000", "key"),
            SecretRef::ai("550e8400-e29b-41d4-a716-446655440000")
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
