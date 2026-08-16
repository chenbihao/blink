//! 桌面便签域（0.16.7）。
//!
//! 架构定位：domain 层负责业务规则（防抖保存、恢复策略、日志隐私），
//! infra/data/sticky.rs 负责纯 DB 读写。
//!
//! **不 use tauri**——domain 保持框架无关（0.15 收敛铁则）。
//! IPC 桥接在 app/commands/sticky.rs。
//!
//! 设计见 phases/0.16-clipboard-polish.md §3.8-§3.10。

pub use crate::infra::data::sticky::{StickyColor, StickyFormat, StickyNote};

use sqlx::SqlitePool;

/// 便签错误——可序列化，IPC 边界保留 `kind` 字段供前端分类展示（spec §4.1）。
///
/// domain 层用此类型替代 `Result<_, String>`；command 层经
/// `CommandError::from(StickyError)` 投影为稳定 `{code, message, detail, retryable}`
/// wire schema（app/command_error.rs）。
#[derive(Debug, serde::Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum StickyError {
    /// 数据库错误（连接失败 / 约束冲突 / 序列化失败等）。
    #[error("数据库错误: {detail}")]
    Db { detail: String },
    /// 便签不存在（id 无效或已被删除）。
    #[error("便签不存在: {id}")]
    NotFound { id: String },
    /// 便签已在回收站，活跃便签操作不能继续。
    #[error("便签已在回收站: {id}")]
    Trashed { id: String },
    /// 乐观并发冲突，调用方应重新读取后再决定是否更新。
    #[error("便签已被修改: {id}（期望版本 {expected_updated_at}，当前版本 {actual_updated_at}）")]
    Conflict {
        id: String,
        expected_updated_at: i64,
        actual_updated_at: i64,
    },
}

/// 便签正文变更来源；用于前端去重/刷新，不携带正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickyChangeSource {
    UserWindow,
    ContentEditor,
    Capability,
}

impl StickyChangeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserWindow => "sticky",
            Self::ContentEditor => "content-editor",
            Self::Capability => "capability",
        }
    }
}

/// 跨 DB、窗口与事件通知的便签编排错误。
#[derive(Debug, thiserror::Error)]
pub enum StickyWorkflowError {
    #[error(transparent)]
    Sticky(#[from] StickyError),
    #[error("便签界面同步失败: {detail}")]
    SideEffect { detail: String },
}

/// 便签原子关闭工作流结果（0.20.0）。
///
/// `close_note` 在单条 SQL 内原子完成 revision 校验、最终保存和 delete/trash 决策。
/// 空内容 → 物理删除（带 revision 守卫）；非空 → 保存最终内容并移入回收站。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StickyCloseOutcome {
    /// 空便签已物理删除（数据库与回收站均不存在）
    DeletedEmpty,
    /// 非空便签已保存最终内容并移入回收站（可恢复）
    Trashed,
}

impl From<String> for StickyError {
    fn from(s: String) -> Self {
        StickyError::Db { detail: s }
    }
}

/// 便签服务：封装保存和恢复策略。
///
/// **防抖**（§3.9）：前端做输入防抖，500ms 停顿后调后端写库。
/// 后端提供即时写库能力，不额外做防抖——防抖在调用方（前端 JS）做更合适，
/// 避免后端持有未保存状态。
///
/// **恢复**（§3.9）：启动时异步读取所有便签，`visible=true` 的恢复窗口，
/// `visible=false` 只进入管理界面。恢复在主窗口服务 ready 后走旁路，
/// 不阻塞 Alt+Space。单条恢复失败只记录并跳过。
pub struct StickyService {
    history_pool: SqlitePool,
}

impl StickyService {
    pub fn new(history_pool: SqlitePool) -> Self {
        Self { history_pool }
    }

    /// 创建新便签。
    pub async fn create_note(
        &self,
        content: &str,
        color: StickyColor,
    ) -> Result<StickyNote, StickyError> {
        let color_str = color.as_str().to_string();
        let note = StickyNote {
            id: crate::infra::data::sticky::generate_id(),
            content: content.to_string(),
            format: StickyFormat::default(),
            color,
            visible: true,
            x: 0,
            y: 0,
            width: crate::infra::data::sticky::DEFAULT_WIDTH,
            height: crate::infra::data::sticky::DEFAULT_HEIGHT,
            always_on_top: true,
            created_at: 0,
            updated_at: 0,
            trashed: false,
            deleted_at: None,
        };
        crate::infra::data::sticky::create(&self.history_pool, &note)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(sticky_id = %note.id, color = %color_str, "便签已创建");
        Ok(note)
    }

    /// 获取便签。
    pub async fn get_note(&self, id: &str) -> Option<StickyNote> {
        crate::infra::data::sticky::get(&self.history_pool, id).await
    }

    /// 获取一条活跃便签，区分不存在与已回收。
    pub async fn get_active_note(&self, id: &str) -> Result<StickyNote, StickyError> {
        let note = crate::infra::data::sticky::get_result(&self.history_pool, id)
            .await
            .map_err(|detail| StickyError::Db { detail })?
            .ok_or_else(|| StickyError::NotFound { id: id.to_string() })?;
        if note.trashed {
            return Err(StickyError::Trashed { id: id.to_string() });
        }
        Ok(note)
    }

    /// 列出全部便签。
    pub async fn list_notes(&self) -> Vec<StickyNote> {
        crate::infra::data::sticky::list(&self.history_pool).await
    }

    /// 列出回收站中的便签（0.17.7）。
    pub async fn list_trashed_notes(&self) -> Vec<StickyNote> {
        crate::infra::data::sticky::list_trashed(&self.history_pool).await
    }

    /// 更新正文；传入 revision 时执行乐观并发校验，返回新的 revision。
    pub async fn update_content(
        &self,
        id: &str,
        content: &str,
        expected_updated_at: Option<i64>,
    ) -> Result<i64, StickyError> {
        use crate::infra::data::sticky::StickyWriteOutcome;

        let outcome = crate::infra::data::sticky::update_content(
            &self.history_pool,
            id,
            content,
            expected_updated_at,
        )
        .await
        .map_err(|e| StickyError::Db { detail: e })?;

        match outcome {
            StickyWriteOutcome::Applied { updated_at } => Ok(updated_at),
            StickyWriteOutcome::NotFound => Err(StickyError::NotFound { id: id.to_string() }),
            StickyWriteOutcome::Trashed => Err(StickyError::Trashed { id: id.to_string() }),
            StickyWriteOutcome::Conflict { actual_updated_at } => Err(StickyError::Conflict {
                id: id.to_string(),
                expected_updated_at: expected_updated_at.unwrap_or_default(),
                actual_updated_at,
            }),
        }
    }

    /// 更新便签外观（颜色 + 可选格式）。返回新的 `updated_at`。
    pub async fn update_appearance(
        &self,
        id: &str,
        color: StickyColor,
        format: Option<StickyFormat>,
    ) -> Result<i64, StickyError> {
        crate::infra::data::sticky::update_appearance(
            &self.history_pool,
            id,
            &color,
            format.as_ref(),
        )
        .await
        .map_err(|e| StickyError::Db { detail: e })?;
        let note = crate::infra::data::sticky::get(&self.history_pool, id)
            .await
            .ok_or_else(|| StickyError::NotFound { id: id.to_string() })?;
        tracing::debug!(sticky_id = %id, color = %color.as_str(), "便签外观已更新");
        Ok(note.updated_at)
    }

    /// 更新便签窗口几何。返回新的 `updated_at`。
    pub async fn update_geometry(
        &self,
        id: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<i64, StickyError> {
        use crate::infra::data::sticky::StickyWriteOutcome;

        let outcome = crate::infra::data::sticky::update_geometry(
            &self.history_pool,
            id,
            x,
            y,
            width,
            height,
        )
        .await
        .map_err(|detail| StickyError::Db { detail })?;
        match outcome {
            StickyWriteOutcome::Applied { updated_at } => Ok(updated_at),
            StickyWriteOutcome::NotFound => Err(StickyError::NotFound { id: id.to_string() }),
            StickyWriteOutcome::Trashed => Err(StickyError::Trashed { id: id.to_string() }),
            StickyWriteOutcome::Conflict { .. } => Err(StickyError::Db {
                detail: format!("便签几何更新状态冲突: {id}"),
            }),
        }
    }

    /// 设置便签可见性。
    ///
    /// 返回新的 `updated_at`（P0-1：前端 mutation queue 需要 revision 跟踪）。
    pub async fn set_visible(&self, id: &str, visible: bool) -> Result<i64, StickyError> {
        let updated_at = crate::infra::data::sticky::set_visible(&self.history_pool, id, visible)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(sticky_id = %id, visible, "便签可见性已变更");
        Ok(updated_at)
    }

    /// 将便签移入回收站（软删除，0.17.7）。
    ///
    /// `trashed=true` + `deleted_at=now`，保留数据。调用后窗口应 hide。
    pub async fn trash_note(&self, id: &str) -> Result<(), StickyError> {
        let outcome = crate::infra::data::sticky::set_trashed(&self.history_pool, id, true)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        match outcome {
            crate::infra::data::sticky::StickyWriteOutcome::Applied { .. } => {}
            crate::infra::data::sticky::StickyWriteOutcome::NotFound => {
                return Err(StickyError::NotFound { id: id.to_string() });
            }
            crate::infra::data::sticky::StickyWriteOutcome::Trashed => {
                return Err(StickyError::Trashed { id: id.to_string() });
            }
            crate::infra::data::sticky::StickyWriteOutcome::Conflict { .. } => unreachable!(),
        }
        tracing::info!(sticky_id = %id, "便签已移入回收站");
        Ok(())
    }

    /// 从回收站恢复便签（0.17.7）。
    ///
    /// `trashed=false` + `deleted_at=null`，恢复到桌面。
    pub async fn restore_note(&self, id: &str) -> Result<(), StickyError> {
        let outcome = crate::infra::data::sticky::set_trashed(&self.history_pool, id, false)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        match outcome {
            crate::infra::data::sticky::StickyWriteOutcome::Applied { .. } => {}
            crate::infra::data::sticky::StickyWriteOutcome::NotFound => {
                return Err(StickyError::NotFound { id: id.to_string() });
            }
            // 已是活跃状态时恢复是幂等成功。
            crate::infra::data::sticky::StickyWriteOutcome::Conflict { .. } => {}
            crate::infra::data::sticky::StickyWriteOutcome::Trashed => unreachable!(),
        }
        tracing::info!(sticky_id = %id, "便签已从回收站恢复");
        Ok(())
    }

    /// 清空回收站（0.17.7）。返回删除的行数。
    pub async fn clear_trashed(&self) -> Result<u64, StickyError> {
        let count = crate::infra::data::sticky::clear_all_trashed(&self.history_pool)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(deleted = count, "回收站已清空");
        Ok(count)
    }

    /// 清理过期回收站便签（0.17.7）。启动时调用。
    #[allow(dead_code)] // 启动清理在 pools.rs 直接调 data 层
    pub async fn cleanup_trashed(&self, retention_days: i64) -> u64 {
        crate::infra::data::sticky::cleanup_trashed(&self.history_pool, retention_days).await
    }

    /// 设置便签置顶。
    ///
    /// 返回新的 `updated_at`（P0-1：前端 mutation queue 需要 revision 跟踪）。
    pub async fn set_always_on_top(
        &self,
        id: &str,
        always_on_top: bool,
    ) -> Result<i64, StickyError> {
        let updated_at =
            crate::infra::data::sticky::set_always_on_top(&self.history_pool, id, always_on_top)
                .await
                .map_err(|e| StickyError::Db { detail: e })?;
        Ok(updated_at)
    }

    /// 原子关闭便签（0.20.0，0.20.7 修订）。
    ///
    /// 在单条 SQL 内原子完成：revision 校验 → 最终内容保存 → delete/trash 决策。
    /// - final content 经 Unicode whitespace trim 后为空 → 物理删除（带 revision 守卫，
    ///   过期版本返回 `Conflict` 而非删除，防止误删他方刚写入的非空内容）
    /// - 非空 → 保存最终内容并移入回收站（无"已保存但未进回收站"中间态）
    ///
    /// `expected_updated_at` 用于乐观并发校验（与 `update_content` 同语义）。
    /// 返回 `StickyCloseOutcome` 表示最终状态；冲突/存储失败走 `StickyError`。
    pub async fn close_note(
        &self,
        id: &str,
        final_content: &str,
        expected_updated_at: Option<i64>,
    ) -> Result<StickyCloseOutcome, StickyError> {
        use crate::infra::data::sticky::{StickyCloseDbOutcome, StickyWriteOutcome};

        let outcome = crate::infra::data::sticky::close_note(
            &self.history_pool,
            id,
            final_content,
            expected_updated_at,
        )
        .await
        .map_err(|e| StickyError::Db { detail: e })?;

        match outcome {
            Ok(StickyCloseDbOutcome::DeletedEmpty) => {
                tracing::info!(sticky_id = %id, "便签已原子关闭（空→删除）");
                Ok(StickyCloseOutcome::DeletedEmpty)
            }
            Ok(StickyCloseDbOutcome::Trashed) => {
                tracing::info!(
                    sticky_id = %id,
                    content_len = final_content.len(),
                    "便签已原子关闭（非空→回收站）"
                );
                Ok(StickyCloseOutcome::Trashed)
            }
            Err(StickyWriteOutcome::NotFound) => Err(StickyError::NotFound { id: id.to_string() }),
            Err(StickyWriteOutcome::Trashed) => Err(StickyError::Trashed { id: id.to_string() }),
            // classify_failed_write 只产 NotFound/Trashed/Conflict
            Err(StickyWriteOutcome::Applied { .. }) => {
                unreachable!("close_note 失败分类不含 Applied")
            }
            Err(StickyWriteOutcome::Conflict { actual_updated_at }) => Err(StickyError::Conflict {
                id: id.to_string(),
                expected_updated_at: expected_updated_at.unwrap_or_default(),
                actual_updated_at,
            }),
        }
    }

    /// 删除便签（永久）。
    pub async fn delete_note(&self, id: &str) -> Result<(), StickyError> {
        crate::infra::data::sticky::delete(&self.history_pool, id)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(sticky_id = %id, "便签已删除");
        Ok(())
    }

    /// 获取便签统计。
    pub async fn get_stats(&self) -> serde_json::Value {
        crate::infra::data::sticky::get_stats(&self.history_pool).await
    }

    /// 恢复服务：启动时异步加载所有便签。
    ///
    /// 返回 `trashed=false && visible=true` 的便签列表（需恢复窗口）。
    /// 回收站中的便签（`trashed=true`）不恢复窗口，只在管理界面显示。
    ///
    /// **单条失败隔离**：某条便签读取失败只记录 warn，不阻断其他便签。
    pub async fn load_for_recovery(&self) -> Vec<StickyNote> {
        let all = crate::infra::data::sticky::list(&self.history_pool).await;
        let total = all.len();
        let visible: Vec<_> = all.into_iter().filter(|n| n.visible).collect();
        let visible_count = visible.len();
        tracing::info!(total, visible_count, "便签恢复：加载完成");
        visible
    }
}

// ── 0.20.0 derive_sticky_title ──────────────────────────────────────────────
//
// dead_code 说明：这组 Markdown 清洗 + 标题派生函数是 0.20.0 为便签窗口标题
// 准备的，当前前端走 content 截断显示标题，后端 derive 路径待消费。
// 函数互相调用组成内部闭环，整体标记 dead_code。

/// 便签标题最大字符数。
#[allow(dead_code)]
const STICKY_TITLE_MAX_CHARS: usize = 48;

/// 从便签内容派生窗口标题（0.20.0）。
///
/// 规则：
/// 1. 取第一条非空行
/// 2. 剥离常见 Markdown 前缀（`#`/`-`/`*`/`>`/`1.` 等）和行内标记（`**bold**`/`*italic*`/`~~strike~~`/`` `code` `` 等）
/// 3. 按 Unicode 字符安全截断到 48 字符
/// 4. 空内容回退本地化"便签"（locale 为 "zh" 时返回中文，其他返回 "Sticky"）
///
/// **安全要求**：对中文、emoji、组合字符安全截断（按 char，不按 byte）。
#[allow(dead_code)] // 0.20.0 标题派生：前端当前走 content 截断，后端路径待消费
pub fn derive_sticky_title(content: &str, locale: &str) -> String {
    // 1. 取第一条非空行
    let first_line = content
        .lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace()))
        .find(|l| !l.is_empty())
        .unwrap_or("");

    // 2. 剥离 Markdown 前缀和行内标记
    let cleaned = strip_markdown(first_line);

    // 3. 截断
    let title: String = cleaned.chars().take(STICKY_TITLE_MAX_CHARS).collect();

    // 4. 空回退
    if title.is_empty() {
        return fallback_title(locale);
    }

    title
}

/// 剥离常见 Markdown 标记，返回纯文本。
#[allow(dead_code)] // 内部函数，被 derive_sticky_title 调用
fn strip_markdown(s: &str) -> String {
    let mut result = s.to_string();

    // 剥离行首 Markdown 前缀（标题 `#`/`##`/`###`、引用 `>`、列表 `-`/`*`/`+`、有序 `1.`/`2.`）
    result = strip_leading_md_prefix(&result);

    // 剥离行内标记
    // **bold** / __bold__
    result = strip_inline_pairs(&result, "**");
    result = strip_inline_pairs(&result, "__");
    // *italic* / _italic_（单字符，谨慎处理）
    result = strip_inline_pairs(&result, "*");
    result = strip_inline_pairs(&result, "_");
    // ~~strike~~
    result = strip_inline_pairs(&result, "~~");
    // `code` / ```code```
    result = strip_inline_pairs(&result, "`");

    // 剥离 Markdown 链接 [text](url) → text
    strip_md_links(&result)
}

/// 剥离行首 Markdown 前缀。
#[allow(dead_code)] // 内部函数，被 strip_markdown 调用
fn strip_leading_md_prefix(s: &str) -> String {
    let trimmed = s.trim_start();
    // 标题前缀 # / ## / ###
    if let Some(rest) = strip_heading(trimmed) {
        return rest.trim_start().to_string();
    }
    // 引用 >
    if trimmed.starts_with('>') {
        return trimmed[1..].trim_start().to_string();
    }
    // 任务列表 - [ ] / - [x]（先于无序列表检查，因为也以 - 开头）
    if let Some(rest) = strip_task_list(trimmed) {
        return rest.trim_start().to_string();
    }
    // 无序列表 - / * / +
    if (trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ "))
        || trimmed == "-"
        || trimmed == "*"
        || trimmed == "+"
    {
        return trimmed[1..].trim_start().to_string();
    }
    // 有序列表 1. / 2. 等
    if let Some(rest) = strip_ordered_list(trimmed) {
        return rest.trim_start().to_string();
    }
    trimmed.to_string()
}

/// 剥离 `#`/`##`/`###` 等标题前缀，返回剩余内容。
#[allow(dead_code)] // 内部函数
fn strip_heading(s: &str) -> Option<String> {
    let hash_count = s.chars().take_while(|&c| c == '#').count();
    if hash_count > 0 && hash_count <= 6 {
        let rest = &s[hash_count..];
        // 必须后跟空格或到行尾才算标题
        if rest.is_empty() || rest.starts_with(' ') {
            return Some(rest.to_string());
        }
    }
    None
}

/// 剥离有序列表前缀 `1.` / `10.` 等。
#[allow(dead_code)] // 内部函数
fn strip_ordered_list(s: &str) -> Option<String> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = &s[digits.len()..];
    if rest.starts_with('.') || rest.starts_with(')') {
        return Some(rest[1..].to_string());
    }
    None
}

/// 剥离任务列表前缀 `- [ ]` / `- [x]` / `* [ ]` 等。
#[allow(dead_code)] // 内部函数
fn strip_task_list(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= 5 {
        // - [ ] 或 - [x]
        if (chars[0] == '-' || chars[0] == '*' || chars[0] == '+')
            && chars[1] == ' '
            && chars[2] == '['
            && (chars[3] == ' ' || chars[3].is_ascii_alphabetic())
            && chars[4] == ']'
        {
            return Some(s.chars().skip(5).collect());
        }
    }
    None
}

/// 剥离行内标记对（如 `**bold**` → `bold`）。
/// 移除所有出现的 marker 对，保留中间文本。
#[allow(dead_code)] // 内部函数
fn strip_inline_pairs(s: &str, marker: &str) -> String {
    let marker_chars: Vec<char> = marker.chars().collect();
    let chars: Vec<char> = s.chars().collect();
    let mlen = marker_chars.len();
    let mut result = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        // 检查是否匹配 marker
        if i + mlen <= chars.len() && chars[i..i + mlen] == marker_chars[..] {
            // 找到匹配的闭合 marker
            if let Some(end) = find_closing_marker(&chars, i + mlen, &marker_chars) {
                // 提取中间文本
                for c in &chars[i + mlen..end] {
                    result.push(*c);
                }
                i = end + mlen;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// 从 start 开始查找匹配的闭合 marker。
#[allow(dead_code)] // 内部函数
fn find_closing_marker(chars: &[char], start: usize, marker: &[char]) -> Option<usize> {
    let mlen = marker.len();
    let mut i = start;
    while i + mlen <= chars.len() {
        if chars[i..i + mlen] == *marker {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 剥离 Markdown 链接 `[text](url)` → `text`。
#[allow(dead_code)] // 内部函数
fn strip_md_links(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            // 查找 ]
            if let Some(bracket_end) = find_char(&chars, i + 1, ']') {
                // 检查后面是否跟 (
                if bracket_end + 1 < chars.len() && chars[bracket_end + 1] == '(' {
                    // 提取 [text] 中的 text
                    for c in &chars[i + 1..bracket_end] {
                        result.push(*c);
                    }
                    // 跳过 [text](url)
                    if let Some(paren_end) = find_char(&chars, bracket_end + 2, ')') {
                        i = paren_end + 1;
                        continue;
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// 从 start 开始查找目标字符。
#[allow(dead_code)] // 内部函数
fn find_char(chars: &[char], start: usize, target: char) -> Option<usize> {
    (start..chars.len()).find(|&i| chars[i] == target)
}

/// 本地化回退标题。
#[allow(dead_code)] // 内部函数
fn fallback_title(locale: &str) -> String {
    match locale {
        "zh" => "便签".to_string(),
        _ => "Sticky".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_title_empty() {
        assert_eq!(derive_sticky_title("", "zh"), "便签");
        assert_eq!(derive_sticky_title("", "en"), "Sticky");
        assert_eq!(derive_sticky_title("   \n  \n  ", "zh"), "便签");
    }

    #[test]
    fn derive_title_plain_text() {
        assert_eq!(derive_sticky_title("hello world", "zh"), "hello world");
        assert_eq!(derive_sticky_title("你好世界", "zh"), "你好世界");
    }

    #[test]
    fn derive_title_first_non_empty_line() {
        assert_eq!(derive_sticky_title("\n\nhello\nworld", "zh"), "hello");
        assert_eq!(derive_sticky_title("  \n  first line", "zh"), "first line");
    }

    #[test]
    fn derive_title_strips_heading() {
        assert_eq!(derive_sticky_title("# Title", "zh"), "Title");
        assert_eq!(derive_sticky_title("## Subtitle", "zh"), "Subtitle");
        assert_eq!(derive_sticky_title("### H3", "zh"), "H3");
    }

    #[test]
    fn derive_title_strips_list() {
        assert_eq!(derive_sticky_title("- item", "zh"), "item");
        assert_eq!(derive_sticky_title("* item", "zh"), "item");
        assert_eq!(derive_sticky_title("+ item", "zh"), "item");
        assert_eq!(derive_sticky_title("1. first", "zh"), "first");
        assert_eq!(derive_sticky_title("10. tenth", "zh"), "tenth");
    }

    #[test]
    fn derive_title_strips_task_list() {
        assert_eq!(derive_sticky_title("- [ ] todo", "zh"), "todo");
        assert_eq!(derive_sticky_title("- [x] done", "zh"), "done");
        assert_eq!(derive_sticky_title("* [ ] task", "zh"), "task");
    }

    #[test]
    fn derive_title_strips_bold() {
        assert_eq!(derive_sticky_title("**bold**", "zh"), "bold");
        assert_eq!(
            derive_sticky_title("text **bold** end", "zh"),
            "text bold end"
        );
        assert_eq!(derive_sticky_title("__bold__", "zh"), "bold");
    }

    #[test]
    fn derive_title_strips_italic() {
        assert_eq!(derive_sticky_title("*italic*", "zh"), "italic");
        assert_eq!(
            derive_sticky_title("text *italic* end", "zh"),
            "text italic end"
        );
    }

    #[test]
    fn derive_title_strips_strike() {
        assert_eq!(derive_sticky_title("~~strike~~", "zh"), "strike");
        assert_eq!(
            derive_sticky_title("text ~~strike~~ end", "zh"),
            "text strike end"
        );
    }

    #[test]
    fn derive_title_strips_code() {
        assert_eq!(derive_sticky_title("`code`", "zh"), "code");
        assert_eq!(
            derive_sticky_title("text `code` end", "zh"),
            "text code end"
        );
    }

    #[test]
    fn derive_title_strips_links() {
        assert_eq!(derive_sticky_title("[text](url)", "zh"), "text");
        assert_eq!(
            derive_sticky_title("see [link](http://x.com)", "zh"),
            "see link"
        );
    }

    #[test]
    fn derive_title_strips_quote() {
        assert_eq!(derive_sticky_title("> quote", "zh"), "quote");
        assert_eq!(derive_sticky_title(">> nested", "zh"), "> nested");
    }

    #[test]
    fn derive_title_truncates_long_text() {
        let long = "a".repeat(100);
        let title = derive_sticky_title(&long, "zh");
        assert_eq!(title.chars().count(), 48);
        assert_eq!(title, "a".repeat(48));
    }

    #[test]
    fn derive_title_truncates_long_chinese() {
        let long = "你".repeat(100);
        let title = derive_sticky_title(&long, "zh");
        assert_eq!(title.chars().count(), 48);
    }

    #[test]
    fn derive_title_emoji_safe() {
        let title = derive_sticky_title("🎉🚀✨ party time", "zh");
        assert_eq!(title, "🎉🚀✨ party time");
    }

    #[test]
    fn derive_title_combined_markdown() {
        assert_eq!(derive_sticky_title("# **Bold Title**", "zh"), "Bold Title");
        assert_eq!(derive_sticky_title("- [x] ~~deleted~~", "zh"), "deleted");
        assert_eq!(
            derive_sticky_title("## [Link Title](http://x.com)", "zh"),
            "Link Title"
        );
    }

    #[test]
    fn derive_title_truncates_markdown_then_plain() {
        // 先剥离 markdown，再截断
        let long_bold = format!("**{}**", "a".repeat(100));
        let title = derive_sticky_title(&long_bold, "zh");
        assert_eq!(title.chars().count(), 48);
        assert_eq!(title, "a".repeat(48));
    }
}
