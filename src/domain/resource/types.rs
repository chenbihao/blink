//! ResourceStore 协议类型（0.23.12）。
//!
//! 详见 `mod.rs` 模块文档。此处只定义数据形状，不含行为。

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;

use crate::infra::platform::file_identity::FileIdentity;

// ── ResourceRef ─────────────────────────────────────────────────────────────

/// opaque 资源引用——跨 Capability 传递大资源的短期授权句柄。
///
/// wire 上是纯字符串（serde transparent）；token 由 OS CSPRNG 生成
/// （`rref_` + 32 个 hex 字符 = 128 位熵），不含路径、字节或可逆推导信息。
/// AI/MCP 可持有 ref（bearer capability），但只能消费签发方授予的 use。
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ResourceRef(pub(crate) String);

impl ResourceRef {
    /// 从原始 token 构造（不做格式强校验——解析失败由 store 的 open 报告）。
    pub fn from_token(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// 原始 token 字符串（仅限可信边界内使用；日志建议只记长度）。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResourceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── ResourceUse ─────────────────────────────────────────────────────────────

/// 资源用途封闭枚举——所有 ref 的通用授权属性（0.23.12 取代 audio_ref 的
/// 任意字符串 scope）。
///
/// **收敛摩擦是有意的**：每新增消费型 Capability 需扩枚举并评审签发方白名单，
/// 不得当债优化掉。无消费方的变体不占位（ReadText 随 read_text_resource
/// 立项时再加入）。
///
/// 首版变体与消费方：
/// - `TranscribeAudio` — `transcribe_audio` Capability / VAD 调试回放分析 /
///   CLI `blink transcribe`
/// - `PreviewAudio` — `read_audio_for_playback`（设置页回放）
/// - `DecodeImage` — `write_clipboard` 图片模式 / `analyze_image_palette`（0.23.12）
/// - `OcrImage` — `ocr_image`（0.23.12）
/// - `PinImage` — `pin_image`（0.23.12）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceUse {
    /// 解码图片（写剪贴板 / 配色分析）。
    DecodeImage,
    /// OCR 文字识别。
    OcrImage,
    /// 钉图（pin 到桌面）。
    PinImage,
    /// 转写音频（喂 STT 引擎）。
    TranscribeAudio,
    /// 试听音频（前端回放字节）。
    PreviewAudio,
}

impl ResourceUse {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DecodeImage => "decode_image",
            Self::OcrImage => "ocr_image",
            Self::PinImage => "pin_image",
            Self::TranscribeAudio => "transcribe_audio",
            Self::PreviewAudio => "preview_audio",
        }
    }
}

impl std::fmt::Display for ResourceUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// use 集合——签发方授予的用途位图（最多 8 个变体，u8 支撑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceUseSet(u8);

impl ResourceUseSet {
    /// 单 use 集合。
    pub const fn single(use_: ResourceUse) -> Self {
        Self(1 << (use_ as u8))
    }

    /// 并集。
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn contains(&self, use_: ResourceUse) -> bool {
        self.0 & (1 << (use_ as u8)) != 0
    }

    /// 判断 self 是否覆盖 other 的全部 use（other ⊆ self）——派生衰减校验用。
    pub fn covers(&self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 空 use 集——签发方构造 grant 时必须至少授予一个 use。
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// 遍历已授予的 use（枚举声明序）。
    pub fn iter(&self) -> impl Iterator<Item = ResourceUse> {
        [
            ResourceUse::DecodeImage,
            ResourceUse::OcrImage,
            ResourceUse::PinImage,
            ResourceUse::TranscribeAudio,
            ResourceUse::PreviewAudio,
        ]
        .into_iter()
        .filter(move |u| self.contains(*u))
    }
}

impl From<ResourceUse> for ResourceUseSet {
    fn from(use_: ResourceUse) -> Self {
        Self::single(use_)
    }
}

// ── ReusePolicy ─────────────────────────────────────────────────────────────

/// 复用策略——grant 属性，不再由媒体类型隐含（0.23 §8.2 决策 2）。
///
/// - 转写默认 `OneShot`（一次性授权）；VAD 调试的双用途走 `issue_from_ref`
///   派生，不靠预先签发 `MaxReads(2)` 弱化转写 one-shot。
/// - 投影层签发的 image_ref 沿用 `Reusable`（先 OCR 再 pin）。
/// - chat 音频附件用 `Reusable` + 会话结束撤销（转写失败重试不烧 ref）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReusePolicy {
    /// 非消费读取——不限次数（TTL/撤销仍生效）。
    Reusable,
    /// 一次性授权——首次成功 open 即消费。
    OneShot,
    /// 最多 n 次成功 open。
    ///
    /// 协议定义于 0.23 §8.2 决策 2（ReusePolicy 三变体）；当前生产签发方
    /// 未使用（VAD 双用途走 issue_from_ref 派生），语义由测试钉死。
    #[allow(dead_code)]
    MaxReads(u32),
}

// ── ResourceBacking ─────────────────────────────────────────────────────────

/// 冻结的本地文件身份——签发时捕获，open 时复核。
///
/// `canonical_path` 仅在 store 内部使用，不出现在 Debug 输出、日志或
/// 返回给非可信调用方的结构中。
#[derive(Clone)]
pub struct FrozenLocalFile {
    pub(crate) canonical_path: PathBuf,
    pub(crate) size: u64,
    pub(crate) identity: FileIdentity,
}

// 手写 Debug：不含 canonical_path（隐私铁则）。
impl std::fmt::Debug for FrozenLocalFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrozenLocalFile")
            .field("size", &self.size)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// 远程对象 key——只留类型边界，本阶段不实现（SSRF/下载炸弹/凭据问题不背）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteObjectKey(pub String);

/// 资源后端——双模落地。
#[derive(Debug, Clone)]
pub enum ResourceBacking {
    /// 内存字节（Arc-backed `Bytes`，clone 零拷贝）。
    Memory(Bytes),
    /// 本地文件（磁盘路径句柄 + FileIdentity 校验）。
    LocalFile(FrozenLocalFile),
    /// 远程对象（未实现——issue/open 返回 `UnsupportedBacking`）。
    #[allow(dead_code)]
    Remote(RemoteObjectKey),
}

/// backing 种类的稳定字符串（诊断/元数据用）。
pub const BACKING_KIND_MEMORY: &str = "memory";
pub const BACKING_KIND_LOCAL_FILE: &str = "local_file";
pub const BACKING_KIND_REMOTE: &str = "remote";

impl ResourceBacking {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Memory(_) => BACKING_KIND_MEMORY,
            Self::LocalFile(_) => BACKING_KIND_LOCAL_FILE,
            Self::Remote(_) => BACKING_KIND_REMOTE,
        }
    }

    /// 逻辑体积（内存字节数或文件 size）——配额计数口径。
    pub fn size_bytes(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::LocalFile(frozen) => frozen.size,
            Self::Remote(_) => 0,
        }
    }
}

// ── Grant / Spec / Metadata ─────────────────────────────────────────────────

/// 签发后的完整 grant——条目内保存的真源。
#[derive(Debug, Clone)]
pub struct ResourceGrant {
    /// 授予的 use 集合。
    pub uses: ResourceUseSet,
    /// 复用策略。
    pub reuse: ReusePolicy,
    /// 撤销组——`revoke_group` 的句柄（取代旧 generation 机制）。
    pub group: u64,
    /// 所有者标签——`revoke_by_owner` 的句柄（如 `chat_attach:<conv_id>`）。
    pub owner: String,
}

/// 签发请求——调用方提交的 grant 规格。
///
/// `group: None` → store 自动分配新组；`ttl_override` 用于会话级长授权
/// （默认随 backing：Memory 15 分钟 / LocalFile 5 分钟）。
#[derive(Debug, Clone)]
pub struct ResourceGrantSpec {
    pub uses: ResourceUseSet,
    pub reuse: ReusePolicy,
    pub owner: String,
    pub group: Option<u64>,
    pub ttl_override: Option<Duration>,
}

impl ResourceGrantSpec {
    /// 最小构造（自动组 + backing 默认 TTL）。
    pub fn new(
        uses: impl Into<ResourceUseSet>,
        reuse: ReusePolicy,
        owner: impl Into<String>,
    ) -> Self {
        Self {
            uses: uses.into(),
            reuse,
            owner: owner.into(),
            group: None,
            ttl_override: None,
        }
    }

    pub fn with_group(mut self, group: u64) -> Self {
        self.group = Some(group);
        self
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl_override = Some(ttl);
        self
    }
}

/// 非消费检查的元数据投影（`inspect` 返回；无路径、无字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceMetadata {
    /// MIME 类型（Memory 腿签发方提供；LocalFile 腿可能为 None）。
    pub mime: Option<String>,
    /// 逻辑体积（字节）。
    pub size_bytes: u64,
    /// 剩余秒数（向下取整，最小 0）。
    pub expires_in_seconds: u64,
    /// backing 种类稳定字符串。
    pub backing_kind: &'static str,
    /// 授予的 use 名列表（诊断用）。
    pub uses: Vec<&'static str>,
}

// ── OpenedResource ──────────────────────────────────────────────────────────

/// open 成功取得的 lease——持有已授权的资源内容或文件句柄。
///
/// - `Memory`：`Bytes` clone（零拷贝），调用方持有期间条目被淘汰不受影响。
/// - `LocalFile`：已打开的只读 `File`（TOCTOU 防护——校验与打开同临界区完成，
///   调用方持有期间文件被替换不影响已打开句柄）。
///
/// 不暴露路径字段（编译期保证，平移 0.22.16 既有测试）。
pub struct OpenedResource {
    backing: OpenedBacking,
    size: u64,
    mime: Option<String>,
}

enum OpenedBacking {
    Memory(Bytes),
    LocalFile(std::fs::File),
}

impl std::fmt::Debug for OpenedResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedResource")
            .field("size", &self.size)
            .field(
                "kind",
                &match &self.backing {
                    OpenedBacking::Memory(_) => BACKING_KIND_MEMORY,
                    OpenedBacking::LocalFile(_) => BACKING_KIND_LOCAL_FILE,
                },
            )
            .finish()
    }
}

impl OpenedResource {
    pub(crate) fn memory(bytes: Bytes, mime: Option<String>) -> Self {
        let size = bytes.len() as u64;
        Self {
            backing: OpenedBacking::Memory(bytes),
            size,
            mime,
        }
    }

    pub(crate) fn local_file(file: std::fs::File, size: u64) -> Self {
        Self {
            backing: OpenedBacking::LocalFile(file),
            size,
            mime: None,
        }
    }

    /// 签发时记录的逻辑体积（字节）——lease 元数据，测试与诊断消费。
    #[allow(dead_code)]
    pub fn size_bytes(&self) -> u64 {
        self.size
    }

    /// MIME 类型（Memory 腿签发方提供；LocalFile 腿通常为 None）。
    pub fn mime(&self) -> Option<&str> {
        self.mime.as_deref()
    }

    /// 有界整读——本阶段唯一落地的读取接口（0.23 §8.2 决策 9；
    /// `read_range`/`open_stream` 只留签名不实现，等真实需求再立项）。
    ///
    /// 超过 `max_bytes` 返回 `BudgetExceeded`，不返回部分数据。
    /// CPU 密集文件读取应在 blocking pool 中调用。
    pub fn read_all_bounded(&mut self, max_bytes: u64) -> Result<Bytes, super::ResourceError> {
        if self.size > max_bytes {
            return Err(super::ResourceError::with_detail(
                super::ResourceErrorKind::ResourceBudgetExceeded,
                format!(
                    "resource size {} exceeds read bound {}",
                    self.size, max_bytes
                ),
            ));
        }
        match &mut self.backing {
            OpenedBacking::Memory(bytes) => Ok(bytes.clone()),
            OpenedBacking::LocalFile(file) => {
                use std::io::Read;
                let mut buffer = Vec::with_capacity(self.size as usize);
                // Take(limit) 保证硬上限——size 与实际内容不一致时也不越界
                file.take(self.size)
                    .read_to_end(&mut buffer)
                    .map_err(super::ResourceError::from_io)
                    .and_then(|_| {
                        if buffer.len() as u64 > max_bytes {
                            Err(super::ResourceError::with_detail(
                                super::ResourceErrorKind::ResourceBudgetExceeded,
                                "file grew beyond read bound",
                            ))
                        } else {
                            Ok(Bytes::from(buffer))
                        }
                    })
            }
        }
    }

    /// `read_range` 预留签名——本阶段不实现（音频转写整读够用；
    /// 文本分页由 `read_text_file` 既有实现承担）。
    #[allow(dead_code)]
    pub fn read_range(&mut self, _offset: u64, _len: u64) -> Result<Bytes, super::ResourceError> {
        Err(super::ResourceError::with_detail(
            super::ResourceErrorKind::UnsupportedBacking,
            "read_range is not implemented in this phase",
        ))
    }
}
