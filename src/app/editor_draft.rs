//! 编辑器恢复草稿存储（本次修复新增）——与 Ctrl+S 完全分离的持久化通道。
//!
//! **动机**：修复前工作正文只存在前端内存中，编辑器崩溃、进程被强杀或系统
//! 强制结束都会丢掉用户正在写的内容。本模块提供**原子、有界、带单调版本水位**
//! 的草稿落盘，崩溃后可恢复：
//!
//! - **原子持久化**：复用 `infra::utils::fs::atomic_write_bytes`（同目录临时文件 +
//!   `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`），进程崩溃不会留下半截文件；
//! - **单调 revision 水位**：同一会话身份（`session_ref + generation`）下，旧
//!   revision 的写入被拒绝——前端防抖/边界 flush 并发乱序时不会用旧正文覆盖新正文；
//!   同一 revision + 同一摘要视为精确重放（幂等，跳过写盘）；
//! - **会话身份切换**：新会话（不同 ref/generation）可接管同一来源键；
//! - **有界**：每个来源键一个文件，写入后按 mtime 保留最新 `max_files` 个，
//!   避免崩溃长尾无限增长；
//! - **不外溢副作用**：本模块只读写草稿文件，绝不触碰剪贴板 / 便签 / 文件目标 /
//!   Capability —— 草稿与保存是两条独立链路。
//!
//! 存储位置：`%APPDATA%\blink\editor-drafts\draft-<keyhash>.json`。
//! 文件名为键摘要（避免非法字符），文件内含原始键与正文，读回时校验键一致。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::domain::editor::{EditorError, MAX_SOURCE_CHARS, body_digest};
use crate::infra::utils::fs::atomic_write_bytes;
use crate::infra::utils::paths;

/// 草稿文件的磁盘形态（同时是 IPC 返回结构，camelCase）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorDraft {
    /// 后端生成的来源草稿键（sticky 稳定；临时来源为不透明实例键）。
    pub key: String,
    /// 写入时冻结的会话引用（诊断用；恢复不要求匹配）。
    #[serde(default)]
    pub session_ref: String,
    /// 写入时冻结的会话代际。
    #[serde(default)]
    pub generation: u64,
    /// 单调内容版本（同会话身份下水位只前进）。
    pub revision: u64,
    /// 正文摘要（与前端同算法：FNV-1a 64 / UTF-8 字节）。
    #[serde(default)]
    pub hash: String,
    /// 正文（UTF-8 真源）。
    #[serde(default)]
    pub body: String,
    /// 草稿产生时的 canonical source 基线摘要；空值表示旧的不可信格式。
    #[serde(default)]
    pub base_digest: String,
    /// 持久来源的 revision（例如 sticky updated_at）。
    #[serde(default)]
    pub base_revision: Option<i64>,
    /// 来源实例身份，供临时来源隔离与诊断。
    #[serde(default)]
    pub source_instance_id: String,
    /// schema 版本；0 表示旧格式，恢复时不得自动覆盖当前正文。
    #[serde(default)]
    pub schema_version: u32,
    /// 来源失效后转存为 orphan，不能再绑定回失效 sticky。
    #[serde(default)]
    pub orphaned: bool,
    /// 落盘时间（ms，诊断与有界清理用）。
    #[serde(default)]
    pub updated_at_ms: i64,
}

/// 内存水位（每个键一份）。
#[derive(Debug, Clone)]
struct Watermark {
    session_ref: String,
    generation: u64,
    revision: u64,
    hash: String,
}

/// 默认保留的草稿文件数（超出按 mtime 淘汰最旧）。
pub const DEFAULT_MAX_DRAFT_FILES: usize = 8;
/// Current on-disk/IPC schema.  Missing/zero versions are legacy and are
/// intentionally treated as untrusted by the frontend recovery policy.
pub const CURRENT_EDITOR_DRAFT_SCHEMA: u32 = 1;

static ARCHIVE_KEY_SEQ: AtomicU64 = AtomicU64::new(1);

/// 恢复草稿存储。由 main.rs `manage`，command 层经 State 取用。
pub struct EditorDraftStore {
    dir: PathBuf,
    /// 写串行门：同一进程内的草稿写入不交错。
    gate: tokio::sync::Mutex<()>,
    /// 键 → 水位（含从磁盘惰性载入的历史水位）。
    watermark: Mutex<HashMap<String, Watermark>>,
    max_chars: usize,
    max_files: usize,
}

impl EditorDraftStore {
    /// 生产构造：`%APPDATA%\blink\editor-drafts`。
    pub fn new() -> Self {
        Self::with_dir(paths::app_data_dir().join("editor-drafts"))
    }

    /// 指定目录构造（单测用，绝不触碰真实用户数据目录）。
    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            dir,
            gate: tokio::sync::Mutex::new(()),
            watermark: Mutex::new(HashMap::new()),
            max_chars: MAX_SOURCE_CHARS,
            max_files: DEFAULT_MAX_DRAFT_FILES,
        }
    }

    /// 仅测试：调整有界上限。
    #[cfg(test)]
    fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }

    /// 保存草稿。返回落盘后的 revision 水位。
    ///
    /// - 同一会话身份且 `revision < 水位` → `StaleRevision`（旧异步写入被拒绝）；
    /// - 同一会话身份、同 revision 且同摘要 → 幂等，跳过写盘；
    /// - 其他情况（前进、新会话接管）→ 真实原子写盘。
    pub async fn save(&self, draft: EditorDraft) -> Result<u64, EditorError> {
        if draft.key.is_empty() {
            return Err(EditorError::Unsupported {
                detail: "草稿键不能为空".into(),
            });
        }
        if draft.body.chars().count() > self.max_chars {
            return Err(EditorError::Unsupported {
                detail: format!("草稿正文超过上限（{} 字符）", self.max_chars),
            });
        }

        // The frontend hash is only an optimization hint.  Recompute it at
        // the persistence boundary so a forged/stale hash cannot become the
        // identity used by the revision wall.
        let draft = EditorDraft {
            hash: format!("{:016x}", body_digest(&draft.body)),
            ..draft
        };

        // 写串行：同一时刻只有一个写入者进入"校验水位 + 落盘"。
        let _gate = self.gate.lock().await;
        self.ensure_watermark(&draft.key).await?;

        {
            let guard = self.lock_watermark()?;
            if let Some(prev) = guard.get(&draft.key) {
                let same_session =
                    prev.session_ref == draft.session_ref && prev.generation == draft.generation;
                if same_session {
                    if draft.revision < prev.revision {
                        tracing::debug!(
                            key = %draft.key,
                            incoming = draft.revision,
                            stored = prev.revision,
                            "editor draft: 旧 revision 写入被拒绝"
                        );
                        return Err(EditorError::StaleRevision);
                    }
                    if draft.revision == prev.revision {
                        // 同水位：同摘要 = 精确重放（幂等跳过）；异正文 = 重放/错位，
                        // 一律拒绝——绝不让不一致的 (revision, 正文) 组合覆盖已落盘内容
                        // （与提交协议 check_commit_revision 同一口径）。
                        if prev.hash == draft.hash {
                            tracing::trace!(key = %draft.key, revision = draft.revision, "editor draft: 幂等重放，跳过写盘");
                            return Ok(prev.revision);
                        }
                        tracing::warn!(
                            key = %draft.key,
                            revision = draft.revision,
                            "editor draft: 同 revision 异正文，拒绝写入"
                        );
                        return Err(EditorError::StaleRevision);
                    }
                }
            }
        }

        let path = self.path_for(&draft.key);
        let payload = EditorDraft {
            updated_at_ms: now_ms(),
            ..draft.clone()
        };
        let bytes = serde_json::to_vec(&payload).map_err(|e| EditorError::Io {
            detail: format!("草稿序列化失败: {e}"),
        })?;
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || atomic_write_bytes(&write_path, &bytes))
            .await
            .map_err(|e| EditorError::Io {
                detail: format!("草稿写入任务失败: {e}"),
            })?
            .map_err(|e| EditorError::Io {
                detail: format!("草稿写入失败: {e}"),
            })?;

        {
            let mut guard = self.lock_watermark()?;
            guard.insert(
                draft.key.clone(),
                Watermark {
                    session_ref: payload.session_ref.clone(),
                    generation: payload.generation,
                    revision: payload.revision,
                    hash: payload.hash.clone(),
                },
            );
        }
        tracing::debug!(
            key = %draft.key,
            revision = draft.revision,
            body_chars = draft.body.chars().count(),
            "editor draft: 已原子落盘"
        );

        self.prune_to_bound().await;
        Ok(draft.revision)
    }

    /// 读取草稿（崩溃恢复）。文件不存在或键不匹配返回 None。
    pub async fn load(&self, key: &str) -> Result<Option<EditorDraft>, EditorError> {
        if key.is_empty() {
            return Ok(None);
        }
        let path = self.path_for(key);
        let read = tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Ok(None);
            }
            let bytes = std::fs::read(&path)?;
            let draft: EditorDraft = serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok::<_, std::io::Error>(Some(draft))
        })
        .await
        .map_err(|e| EditorError::Io {
            detail: format!("草稿读取任务失败: {e}"),
        })?
        .map_err(|e| EditorError::Io {
            detail: format!("草稿读取失败: {e}"),
        })?;

        match read {
            Some(mut draft) if draft.key == key => {
                // Old files may have an absent or incorrect client hash.  The
                // body itself is the authority for the working-text digest.
                draft.hash = format!("{:016x}", body_digest(&draft.body));
                // 顺带把磁盘水位灌入内存，避免重启后旧 revision 覆盖新正文
                let mut guard = self.lock_watermark()?;
                guard.insert(
                    draft.key.clone(),
                    Watermark {
                        session_ref: draft.session_ref.clone(),
                        generation: draft.generation,
                        revision: draft.revision,
                        hash: draft.hash.clone(),
                    },
                );
                Ok(Some(draft))
            }
            // 键不匹配：文件已被同一键摘要的其它内容占用（理论不可达），保守返回 None
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    /// 返回最近的恢复候选。临时来源无法安全关联新会话时由此发现，
    /// 而不是尝试把候选注入其它来源。
    pub async fn list(&self) -> Result<Vec<EditorDraft>, EditorError> {
        let dir = self.dir.clone();
        let max_files = self.max_files;
        tokio::task::spawn_blocking(move || {
            let mut drafts = Vec::new();
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(drafts),
                Err(e) => return Err(e),
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with("draft-") || !name.ends_with(".json") {
                    continue;
                }
                let bytes = match std::fs::read(entry.path()) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
                let Ok(mut draft) = serde_json::from_slice::<EditorDraft>(&bytes) else {
                    continue;
                };
                if draft.key.is_empty() {
                    continue;
                }
                draft.hash = format!("{:016x}", body_digest(&draft.body));
                drafts.push(draft);
            }
            drafts.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
            drafts.truncate(max_files);
            Ok(drafts)
        })
        .await
        .map_err(|e| EditorError::Io {
            detail: format!("草稿候选读取任务失败: {e}"),
        })?
        .map_err(|e| EditorError::Io {
            detail: format!("草稿候选读取失败: {e}"),
        })
    }

    /// 按保存时的完整身份清理草稿。任何字段不匹配都视为迟到 clear，
    /// 不删除当前新会话草稿。
    pub async fn clear_if_matches(
        &self,
        key: &str,
        expected_session_ref: Option<&str>,
        expected_generation: Option<u64>,
        expected_revision: Option<u64>,
        expected_hash: Option<&str>,
    ) -> Result<bool, EditorError> {
        if key.is_empty() {
            return Ok(false);
        }
        let _gate = self.gate.lock().await;
        let Some(current) = self.load(key).await? else {
            return Ok(true);
        };
        let expected_hash = match expected_hash {
            Some(hash) if !hash.is_empty() => hash,
            _ => return Ok(false),
        };
        if expected_session_ref != Some(current.session_ref.as_str())
            || expected_generation != Some(current.generation)
            || expected_revision != Some(current.revision)
            || expected_hash != current.hash.as_str()
        {
            tracing::debug!(key = %key, "editor draft: 拒绝不匹配的迟到 clear");
            return Ok(false);
        }
        let path = self.path_for(key);
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| EditorError::Io {
            detail: format!("草稿清理任务失败: {e}"),
        })?
        .map_err(|e| EditorError::Io {
            detail: format!("草稿清理失败: {e}"),
        })?;

        let mut guard = self.lock_watermark()?;
        guard.remove(key);
        Ok(true)
    }

    /// 标记来源失效后的 orphan 草稿。与 clear 使用同一完整身份墙，
    /// 迟到的旧会话不能给同键新草稿加上 orphan 标记。
    pub async fn mark_orphaned_if_matches(
        &self,
        key: &str,
        expected_session_ref: Option<&str>,
        expected_generation: Option<u64>,
        expected_revision: Option<u64>,
        expected_hash: Option<&str>,
    ) -> Result<bool, EditorError> {
        if key.is_empty() {
            return Ok(false);
        }
        let _gate = self.gate.lock().await;
        let Some(current) = self.load(key).await? else {
            return Ok(false);
        };
        let expected_hash = match expected_hash {
            Some(hash) if !hash.is_empty() => hash,
            _ => return Ok(false),
        };
        if expected_session_ref != Some(current.session_ref.as_str())
            || expected_generation != Some(current.generation)
            || expected_revision != Some(current.revision)
            || expected_hash != current.hash.as_str()
        {
            tracing::debug!(key = %key, "editor draft: 拒绝不匹配的迟到 orphan 标记");
            return Ok(false);
        }
        if current.orphaned {
            return Ok(true);
        }
        let path = self.path_for(key);
        let payload = EditorDraft {
            orphaned: true,
            updated_at_ms: now_ms(),
            ..current
        };
        let bytes = serde_json::to_vec(&payload).map_err(|e| EditorError::Io {
            detail: format!("草稿序列化失败: {e}"),
        })?;
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || atomic_write_bytes(&write_path, &bytes))
            .await
            .map_err(|e| EditorError::Io {
                detail: format!("草稿 orphan 标记任务失败: {e}"),
            })?
            .map_err(|e| EditorError::Io {
                detail: format!("草稿 orphan 标记失败: {e}"),
            })?;
        Ok(true)
    }

    /// 把一个精确匹配的候选迁移到后端生成的独立 orphan 键。
    ///
    /// 用于同键恢复冲突的“暂不处理”：旧候选必须继续可见，但当前会话也要
    /// 能在原键自动保存。先原子写入新文件，再删除旧文件；任何身份墙不匹配
    /// 都返回 None，绝不移动后来写入的正文。
    pub async fn archive_if_matches(
        &self,
        key: &str,
        expected_session_ref: Option<&str>,
        expected_generation: Option<u64>,
        expected_revision: Option<u64>,
        expected_hash: Option<&str>,
    ) -> Result<Option<String>, EditorError> {
        if key.is_empty() {
            return Ok(None);
        }
        let _gate = self.gate.lock().await;
        let Some(current) = self.load(key).await? else {
            return Ok(None);
        };
        let expected_hash = match expected_hash {
            Some(hash) if !hash.is_empty() => hash,
            _ => return Ok(None),
        };
        if expected_session_ref != Some(current.session_ref.as_str())
            || expected_generation != Some(current.generation)
            || expected_revision != Some(current.revision)
            || expected_hash != current.hash.as_str()
        {
            tracing::debug!(key = %key, "editor draft: 拒绝不匹配的迟到 archive");
            return Ok(None);
        }

        let archive_key = loop {
            let candidate = format!(
                "orphan:{}:{}:{}",
                now_ms(),
                std::process::id(),
                ARCHIVE_KEY_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            if !self.path_for(&candidate).exists() {
                break candidate;
            }
        };
        let payload = EditorDraft {
            key: archive_key.clone(),
            orphaned: true,
            updated_at_ms: now_ms(),
            ..current
        };
        let bytes = serde_json::to_vec(&payload).map_err(|e| EditorError::Io {
            detail: format!("草稿归档序列化失败: {e}"),
        })?;
        let archive_path = self.path_for(&archive_key);
        tokio::task::spawn_blocking(move || atomic_write_bytes(&archive_path, &bytes))
            .await
            .map_err(|e| EditorError::Io {
                detail: format!("草稿归档写入任务失败: {e}"),
            })?
            .map_err(|e| EditorError::Io {
                detail: format!("草稿归档写入失败: {e}"),
            })?;

        let old_path = self.path_for(key);
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&old_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| EditorError::Io {
            detail: format!("草稿归档清理任务失败: {e}"),
        })?
        .map_err(|e| EditorError::Io {
            detail: format!("草稿归档清理失败: {e}"),
        })?;

        {
            let mut guard = self.lock_watermark()?;
            guard.remove(key);
            guard.insert(
                archive_key.clone(),
                Watermark {
                    session_ref: payload.session_ref,
                    generation: payload.generation,
                    revision: payload.revision,
                    hash: payload.hash,
                },
            );
        }
        self.prune_to_bound().await;
        Ok(Some(archive_key))
    }

    /// 测试专用无条件清理；生产 IPC 必须使用 `clear_if_matches`。
    #[cfg(test)]
    pub async fn clear_for_test(&self, key: &str) -> Result<(), EditorError> {
        if key.is_empty() {
            return Ok(());
        }
        let path = self.path_for(key);
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| EditorError::Io {
            detail: format!("草稿清理任务失败: {e}"),
        })?
        .map_err(|e| EditorError::Io {
            detail: format!("草稿清理失败: {e}"),
        })?;
        self.lock_watermark()?.remove(key);
        Ok(())
    }

    /// 键 → 草稿文件路径（摘要命名，规避非法字符）
    fn path_for(&self, key: &str) -> PathBuf {
        self.dir
            .join(format!("draft-{:016x}.json", body_digest(key)))
    }

    /// 首次接触某个键时，把磁盘上的水位载入内存（重启后水位不丢失）
    async fn ensure_watermark(&self, key: &str) -> Result<(), EditorError> {
        if self.lock_watermark()?.contains_key(key) {
            return Ok(());
        }
        self.load(key).await?;
        Ok(())
    }

    fn lock_watermark(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Watermark>>, EditorError> {
        self.watermark.lock().map_err(|_| EditorError::Io {
            detail: "草稿水位锁已中毒".into(),
        })
    }

    /// 有界清理：按 mtime 保留最新 `max_files` 个草稿文件
    async fn prune_to_bound(&self) {
        let dir = self.dir.clone();
        let max_files = self.max_files;
        let _ = tokio::task::spawn_blocking(move || prune_dir(&dir, max_files)).await;
    }
}

impl Default for EditorDraftStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 按 mtime 从新到旧保留 `max_files` 个 `draft-*.json`，其余删除。
fn prune_dir(dir: &Path, max_files: usize) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut files: Vec<(i64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("draft-") || !name.ends_with(".json") {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        files.push((mtime, path));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut removed = 0;
    for (_, path) in files.into_iter().skip(max_files) {
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blink-editor-draft-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn draft(key: &str, session: &str, generation: u64, revision: u64, body: &str) -> EditorDraft {
        EditorDraft {
            key: key.into(),
            session_ref: session.into(),
            generation,
            revision,
            hash: format!("{:016x}", body_digest(body)),
            body: body.into(),
            base_digest: format!("{:016x}", body_digest("base")),
            base_revision: Some(1),
            source_instance_id: key.into(),
            schema_version: 1,
            orphaned: false,
            updated_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn save_then_load_preserves_utf8_bytes_exactly() {
        let dir = temp_dir("roundtrip");
        let store = EditorDraftStore::with_dir(dir.clone());
        let body = "第一行\r\n第二行\n\n* 列表\nTitle\n=====\n\n尾部\n\n";

        let stored = store
            .save(draft("sticky:s1", "ed_1", 1, 3, body))
            .await
            .unwrap();
        assert_eq!(stored, 3);

        let loaded = store.load("sticky:s1").await.unwrap().unwrap();
        assert_eq!(loaded.body, body, "正文必须逐字节一致（含 CRLF 与空行）");
        assert_eq!(loaded.revision, 3);
        assert_eq!(loaded.key, "sticky:s1");
        assert!(loaded.updated_at_ms > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn backend_recomputes_body_hash_and_clear_has_identity_wall() {
        let dir = temp_dir("hash-clear-wall");
        let store = EditorDraftStore::with_dir(dir);
        let mut incoming = draft("sticky:s1", "session-a", 1, 7, "正文\r\n");
        incoming.hash = "client-forged-hash".into();
        store.save(incoming).await.unwrap();

        let loaded = store.load("sticky:s1").await.unwrap().unwrap();
        let actual = format!("{:016x}", body_digest("正文\r\n"));
        assert_eq!(loaded.hash, actual, "后端必须以正文重新计算 hash");

        assert!(
            store
                .mark_orphaned_if_matches(
                    "sticky:s1",
                    Some("session-a"),
                    Some(1),
                    Some(7),
                    Some(actual.as_str()),
                )
                .await
                .unwrap()
        );
        assert!(store.load("sticky:s1").await.unwrap().unwrap().orphaned);

        assert!(
            !store
                .clear_if_matches(
                    "sticky:s1",
                    Some("session-a"),
                    Some(1),
                    Some(7),
                    Some("wrong")
                )
                .await
                .unwrap()
        );
        assert!(store.load("sticky:s1").await.unwrap().is_some());
        assert!(
            store
                .clear_if_matches(
                    "sticky:s1",
                    Some("session-a"),
                    Some(1),
                    Some(7),
                    Some(actual.as_str()),
                )
                .await
                .unwrap()
        );
        assert!(store.load("sticky:s1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_body_is_a_real_draft() {
        let dir = temp_dir("empty-body");
        let store = EditorDraftStore::with_dir(dir);
        store
            .save(draft("editor:src-empty", "session-a", 1, 2, ""))
            .await
            .unwrap();
        let loaded = store.load("editor:src-empty").await.unwrap().unwrap();
        assert_eq!(loaded.body, "");
        assert_eq!(loaded.revision, 2);
    }

    #[tokio::test]
    async fn archive_moves_exact_candidate_and_frees_original_key() {
        let dir = temp_dir("archive-conflict");
        let store = EditorDraftStore::with_dir(dir);
        store
            .save(draft("sticky:s1", "old-session", 2, 7, "旧候选"))
            .await
            .unwrap();
        let hash = format!("{:016x}", body_digest("旧候选"));

        let archived_key = store
            .archive_if_matches(
                "sticky:s1",
                Some("old-session"),
                Some(2),
                Some(7),
                Some(&hash),
            )
            .await
            .unwrap()
            .expect("匹配候选应被归档");
        assert!(archived_key.starts_with("orphan:"));
        assert!(store.load("sticky:s1").await.unwrap().is_none());
        let archived = store.load(&archived_key).await.unwrap().unwrap();
        assert_eq!(archived.body, "旧候选");
        assert!(archived.orphaned);

        store
            .save(draft("sticky:s1", "new-session", 3, 1, "当前正文"))
            .await
            .unwrap();
        assert_eq!(
            store.load("sticky:s1").await.unwrap().unwrap().body,
            "当前正文"
        );
        assert_eq!(
            store.load(&archived_key).await.unwrap().unwrap().body,
            "旧候选"
        );
    }

    #[tokio::test]
    async fn stale_revision_write_is_rejected() {
        let dir = temp_dir("stale");
        let store = EditorDraftStore::with_dir(dir.clone());
        store
            .save(draft("empty", "ed_1", 1, 5, "新正文"))
            .await
            .unwrap();

        let err = store
            .save(draft("empty", "ed_1", 1, 4, "旧正文"))
            .await
            .unwrap_err();
        assert!(matches!(err, EditorError::StaleRevision));

        let loaded = store.load("empty").await.unwrap().unwrap();
        assert_eq!(loaded.body, "新正文", "旧 revision 不得覆盖新正文");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exact_replay_is_idempotent_and_skips_rewrite() {
        let dir = temp_dir("idempotent");
        let store = EditorDraftStore::with_dir(dir.clone());
        let first = store
            .save(draft("empty", "ed_1", 1, 2, "同一正文"))
            .await
            .unwrap();
        // 精确重放（响应丢失后的重试）：水位不变且不报错
        let second = store
            .save(draft("empty", "ed_1", 1, 2, "同一正文"))
            .await
            .unwrap();
        assert_eq!(first, second);

        // 同 revision 但正文不同 → 仍是 StaleRevision（重放/错位）
        let err = store
            .save(draft("empty", "ed_1", 1, 2, "被篡改"))
            .await
            .unwrap_err();
        assert!(matches!(err, EditorError::StaleRevision));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn new_session_takes_over_same_key() {
        let dir = temp_dir("takeover");
        let store = EditorDraftStore::with_dir(dir.clone());
        store
            .save(draft("sticky:s1", "ed_1", 1, 9, "旧会话高版本"))
            .await
            .unwrap();

        // 新会话（重启后）revision 从 1 开始：必须被接受，否则永远写不进草稿
        let stored = store
            .save(draft("sticky:s1", "ed_2", 2, 1, "新会话正文"))
            .await
            .unwrap();
        assert_eq!(stored, 1);
        assert_eq!(
            store.load("sticky:s1").await.unwrap().unwrap().body,
            "新会话正文"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn clear_removes_draft_and_watermark() {
        let dir = temp_dir("clear");
        let store = EditorDraftStore::with_dir(dir.clone());
        store
            .save(draft("selection", "ed_1", 1, 2, "正文"))
            .await
            .unwrap();
        assert!(store.load("selection").await.unwrap().is_some());

        store.clear_for_test("selection").await.unwrap();
        assert!(store.load("selection").await.unwrap().is_none());
        // 幂等：重复清理不报错
        store.clear_for_test("selection").await.unwrap();

        // 清理后水位归零：新会话的低 revision 可重新写入
        store
            .save(draft("selection", "ed_2", 3, 1, "新正文"))
            .await
            .unwrap();
        assert_eq!(
            store.load("selection").await.unwrap().unwrap().body,
            "新正文"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keys_with_unsafe_characters_get_safe_filenames() {
        let dir = temp_dir("keys");
        let store = EditorDraftStore::with_dir(dir.clone());
        let key = "sticky:../中文 键/with:colon";
        store.save(draft(key, "ed_1", 1, 1, "正文")).await.unwrap();
        let escaped = store.path_for(key);
        assert_eq!(
            escaped.parent().unwrap(),
            dir.as_path(),
            "文件名不得逃逸目录"
        );
        assert!(
            escaped
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("draft-")
        );
        assert_eq!(store.load(key).await.unwrap().unwrap().body, "正文");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let dir = temp_dir("oversize");
        let store = EditorDraftStore::with_dir(dir.clone());
        let over = "字".repeat(MAX_SOURCE_CHARS + 1);
        let err = store
            .save(draft("empty", "ed_1", 1, 1, &over))
            .await
            .unwrap_err();
        assert!(matches!(err, EditorError::Unsupported { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn drafts_are_bounded_and_have_no_temp_leftovers() {
        let dir = temp_dir("bounded");
        let store = EditorDraftStore::with_dir(dir.clone()).with_max_files(3);
        for i in 0..6 {
            store
                .save(draft(&format!("empty-{i}"), "ed_1", 1, 1, "正文"))
                .await
                .unwrap();
        }
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let drafts = files.iter().filter(|f| f.starts_with("draft-")).count();
        assert!(drafts <= 3, "草稿文件必须有界（实际 {drafts}）");
        assert!(
            !files.iter().any(|f| f.starts_with(".blink-tmp-")),
            "不得残留临时文件：{files:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_key_is_rejected() {
        let dir = temp_dir("empty-key");
        let store = EditorDraftStore::with_dir(dir.clone());
        assert!(matches!(
            store.save(draft("", "ed_1", 1, 1, "x")).await.unwrap_err(),
            EditorError::Unsupported { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
