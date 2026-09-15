//! AudioResourceRegistry 实现——有界 audio_ref 资源注册表（0.22.16 H03）。
//!
//! 详见 `mod.rs` 的模块文档。此处实现签发/解析/淘汰/安全校验全链路。

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::infra::platform::file_identity::{self, FileIdentity};

use super::error::{AudioRefError, AudioRefErrorKind};

/// 默认 TTL：5 分钟。读取不续期。
const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// 默认最大条目数。
const DEFAULT_MAX_ITEMS: usize = 32;

/// 默认累计字节预算（256 MB）。
const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// 默认单项字节上限（128 MB）。
const DEFAULT_MAX_SINGLE_BYTES: u64 = 128 * 1024 * 1024;

/// registry 配置——参数化以便测试。
///
/// TTL、容量与分页大小属于实现/config 参数，按真实负载调整，不在规范中固化。
#[derive(Debug, Clone)]
pub struct AudioResourceConfig {
    /// TTL——签发后多少时间内可解析。
    pub ttl: Duration,
    /// 最大条目数。
    pub max_items: usize,
    /// 累计字节预算。
    pub max_total_bytes: u64,
    /// 单项字节上限。
    pub max_single_bytes: u64,
}

impl Default for AudioResourceConfig {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_TTL,
            max_items: DEFAULT_MAX_ITEMS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_single_bytes: DEFAULT_MAX_SINGLE_BYTES,
        }
    }
}

/// 签发时记录的文件元数据。
#[derive(Debug, Clone)]
struct AudioEntry {
    /// canonical path（仅在内部使用，不出现在 Debug/日志中）。
    #[allow(dead_code)]
    canonical_path: PathBuf,
    /// 文件体积（字节）。
    size: u64,
    /// 平台文件身份。
    identity: FileIdentity,
    /// 签发时间（参与 TTL 校验）。
    issued_at: Instant,
    /// 过期时间——签发时固定，不续期。
    expires_at: Instant,
    /// registry generation（签发时的 generation）。
    generation: u64,
    /// scope 标签——用于跨 scope 拒绝。
    scope: String,
}

/// 解析成功后返回的受控资源。
///
/// 持有已打开的 `File`，避免“校验路径后重新打开”的 TOCTOU。
/// 调用方拿到 `File` 后可以安全读取字节。
///
/// **注意**：解析成功后此 ref 到期，但不中断本次已授权请求。
/// 调用方持有 `OpenedAudioResource` 期间，即使注册表条目被淘汰，
/// 已打开的 file handle 不受影响。
#[derive(Debug)]
pub struct OpenedAudioResource {
    /// 已打开的文件句柄——只读。
    pub file: File,
    /// 文件体积（字节）——签发时记录。
    pub size: u64,
    /// 平台文件身份——签发时记录。
    #[allow(dead_code)]
    pub identity: FileIdentity,
}

/// registry 诊断统计（测试/诊断用，不含敏感信息）。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct RegistryStats {
    #[allow(dead_code)]
    pub entry_count: usize,
    #[allow(dead_code)]
    pub total_bytes: u64,
    #[allow(dead_code)]
    pub generation: u64,
}

/// 有界 audio_ref 资源注册表——线程安全，无后台线程。
///
/// 由 app 层（如 `TauriDomainEnv`）持有，后续 agent 负责 wiring。
/// CLI / MCP 最小运行时不构造。
pub struct AudioResourceRegistry {
    entries: Mutex<HashMap<String, AudioEntry>>,
    /// token 生成计数器——混入时间戳后 xorshift，不可预测。
    counter: AtomicU64,
    /// generation 计数器——每次 `bump_generation` 推进。
    generation: AtomicU64,
    /// 配置。
    config: AudioResourceConfig,
}

impl Default for AudioResourceRegistry {
    fn default() -> Self {
        Self::new(AudioResourceConfig::default())
    }
}

impl AudioResourceRegistry {
    /// 构造空 registry。
    pub fn new(config: AudioResourceConfig) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            config,
        }
    }

    /// 推进 generation——旧 generation 的所有 ref 到期。
    #[allow(dead_code)]
    pub fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 获取当前 generation。
    #[allow(dead_code)]
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 生成不可预测的进程内 token。
    ///
    /// 混合原子计数器 + 系统纳秒时间戳 + 两次 RandomState hash，经 xorshift 打散后 hex 编码。
    /// 不依赖外部 rand crate，足够防猜测（进程内短期 bearer）。
    /// token 不得由路径/hash 可逆推导。
    fn generate_token(&self) -> String {
        use std::hash::{BuildHasher, Hasher};

        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        // xorshift64 打散
        let mut x = seq.wrapping_add(nanos).wrapping_mul(0x9E3779B97F4A7C15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58476D1CE4E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;

        // 第二轮：用 RandomState 混入额外熵
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(x);
        hasher.write_u64(seq.wrapping_add(0xDEAD_BEEF));
        let h2 = hasher.finish();

        format!("aref_{x:016x}{h2:016x}")
    }

    /// 内联清理已过期条目。调用方已持有锁。
    fn evict_expired(entries: &mut HashMap<String, AudioEntry>) {
        let now = Instant::now();
        entries.retain(|_, e| e.expires_at > now);
    }

    /// 计算当前总字节数。调用方已持有锁。
    fn total_bytes(entries: &HashMap<String, AudioEntry>) -> u64 {
        entries.values().map(|e| e.size).sum()
    }

    /// 按最早创建顺序淘汰，直到满足 `max_items` 和 `max_total` 约束。
    fn evict_oldest(entries: &mut HashMap<String, AudioEntry>, config: &AudioResourceConfig) {
        // 先按项数淘汰
        while entries.len() > config.max_items {
            let oldest_key = entries
                .iter()
                .min_by_key(|(_, e)| e.issued_at)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest_key {
                entries.remove(&key);
            } else {
                break;
            }
        }
        // 再按总字节淘汰
        while Self::total_bytes(entries) > config.max_total_bytes {
            let oldest_key = entries
                .iter()
                .min_by_key(|(_, e)| e.issued_at)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest_key {
                entries.remove(&key);
            } else {
                break;
            }
        }
    }

    /// 签发一个 opaque `audio_ref`。
    ///
    /// **前置条件**：
    /// - `abs_path` 是用户通过可信入口显式选择的绝对本地路径
    /// - 路径必须指向一个存在的 regular file
    ///
    /// **签发时记录**：canonical path、size、mtime、平台文件 identity、
    /// registry generation、scope、created/expires。
    ///
    /// **安全**：token 不可预测，不把绝对路径返回给非可信调用方或写入日志。
    /// 成功返回 `Ok(audio_ref)`，失败返回 `AudioRefError`。
    pub fn issue(&self, abs_path: &Path, scope: &str) -> Result<String, AudioRefError> {
        // 1. 验证绝对路径
        if !abs_path.is_absolute() {
            return Err(AudioRefError::with_detail(
                AudioRefErrorKind::NotRegularFile,
                "path is not absolute",
            ));
        }

        // 2. 验证 regular file（拒绝目录、symlink/reparse point）
        if !file_identity::is_regular_file(abs_path) {
            return Err(AudioRefError::new(AudioRefErrorKind::NotRegularFile));
        }

        // 3. 打开文件并获取 identity（避免 TOCTOU：先打开再校验）
        let (file, identity) =
            file_identity::open_file_for_identity(abs_path).map_err(AudioRefError::from)?;

        // 4. 验证单项大小限制
        if identity.size > self.config.max_single_bytes {
            return Err(AudioRefError::with_detail(
                AudioRefErrorKind::ResourceBudgetExceeded,
                format!(
                    "single file size {} exceeds max {}",
                    identity.size, self.config.max_single_bytes
                ),
            ));
        }

        // 5. canonicalize path（内部记录，不返回给调用方）
        let canonical_path = abs_path
            .canonicalize()
            .unwrap_or_else(|_| abs_path.to_path_buf());

        // 6. 生成不可预测 token
        let token = self.generate_token();
        let now = Instant::now();
        let current_gen = self.generation.load(Ordering::SeqCst);

        let entry = AudioEntry {
            canonical_path,
            size: identity.size,
            identity: identity.clone(),
            issued_at: now,
            expires_at: now + self.config.ttl,
            generation: current_gen,
            scope: scope.to_string(),
        };

        // 7. 写入注册表（带惰性清理）
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        Self::evict_expired(&mut entries);
        entries.insert(token.clone(), entry);
        Self::evict_oldest(&mut entries, &self.config);

        // evict_oldest 可能淘汰了刚插入的
        if entries.contains_key(&token) {
            // file handle 不需要保持——解析时重新打开并校验 identity
            drop(file);
            tracing::debug!(
                token_len = token.len(),
                generation = current_gen,
                file_size = identity.size,
                "audio_ref issued"
            );
            Ok(token)
        } else {
            Err(AudioRefError::with_detail(
                AudioRefErrorKind::ResourceBudgetExceeded,
                "evicted immediately after insert",
            ))
        }
    }

    /// 解析 `audio_ref`，返回已打开的 file handle + 身份信息。
    ///
    /// **校验链（按顺序）**：
    /// 1. ref 存在于注册表
    /// 2. generation 未过期（当前 generation）
    /// 3. TTL 未超时
    /// 4. scope 匹配
    /// 5. 文件仍存在且是 regular file
    /// 6. 复核 identity、size、mtime
    ///
    /// **TOCTOU 防护**：解析时直接打开文件获取 identity，
    /// 不走"校验路径后重新打开"。
    ///
    /// **ref 到期语义**：解析成功后从此条目从注册表移除（一次性授权），
    /// 但返回的 `OpenedAudioResource` 不受影响——本次已授权请求不被中断。
    pub fn resolve(
        &self,
        audio_ref: &str,
        scope: &str,
    ) -> Result<OpenedAudioResource, AudioRefError> {
        // 1. 空或格式校验
        if audio_ref.is_empty() || !audio_ref.starts_with("aref_") {
            return Err(AudioRefError::new(AudioRefErrorKind::InvalidAudioRef));
        }

        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());

        // 2. 存在性——先取条目再做过期检查
        //    不先做 evict_expired，否则过期条目会被直接删除，返回 InvalidAudioRef 而非 StaleAudioRef
        let entry = entries
            .remove(audio_ref)
            .ok_else(AudioRefErrorKind::InvalidAudioRef)?;

        // 3. generation 校验
        let current_gen = self.generation.load(Ordering::SeqCst);
        if entry.generation != current_gen {
            // 放回条目？不——一次性授权，已 remove。
            return Err(AudioRefError::with_detail(
                AudioRefErrorKind::GenerationMismatch,
                format!(
                    "ref gen {} != current gen {}",
                    entry.generation, current_gen
                ),
            ));
        }

        // 4. TTL 校验——在 evict 之前检查，返回准确的 StaleAudioRef
        if Instant::now() > entry.expires_at {
            // 惰性清理其他过期条目（即使本次已过期，也顺便清理）
            Self::evict_expired(&mut entries);
            return Err(AudioRefError::new(AudioRefErrorKind::StaleAudioRef));
        }

        // 惰性清理其他过期条目（不影响本次解析）
        Self::evict_expired(&mut entries);

        // 5. scope 校验
        if entry.scope != scope {
            return Err(AudioRefError::with_detail(
                AudioRefErrorKind::ScopeMismatch,
                "scope does not match",
            ));
        }

        // 6. 打开文件并获取 identity
        let (file, current_identity) = file_identity::open_file_for_identity(&entry.canonical_path)
            .map_err(AudioRefError::from)?;

        // 7. 复核 identity
        if !entry.identity.matches(&current_identity) {
            return Err(AudioRefError::with_detail(
                AudioRefErrorKind::FileIdentityChanged,
                "file identity mismatch (size/mtime/volume/index changed)",
            ));
        }

        // 8. 复核 regular file（通过 open 已隐含——但显式复核 reparse point）
        if !file_identity::is_regular_file(&entry.canonical_path) {
            return Err(AudioRefError::new(AudioRefErrorKind::NotRegularFile));
        }

        tracing::debug!(
            token_len = audio_ref.len(),
            generation = entry.generation,
            file_size = entry.size,
            "audio_ref resolved"
        );

        Ok(OpenedAudioResource {
            file,
            size: entry.size,
            identity: entry.identity,
        })
    }

    /// 从一条仍然有效的 ref 派生一条新 ref，**不消费原 ref**。
    ///
    /// 与 `resolve` 的一次性授权不同：同一文件需要两次独立使用时
    /// （如 VAD 调试：引擎分析消费一条、前端取回放字节消费一条），
    /// 先 clone 出同 scope、TTL 重新起算的新 ref，两条 ref 各自一次性消费。
    ///
    /// **校验链**：与 resolve 相同的存在/generation/TTL/scope 检查（不移除原条目），
    /// 随后锁外走 `issue` 的完整校验（重新打开文件 + identity 复核 + 容量预算）签发。
    pub fn clone_audio_ref(&self, audio_ref: &str, scope: &str) -> Result<String, AudioRefError> {
        let canonical_path = {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let entry = entries
                .get(audio_ref)
                .ok_else(AudioRefErrorKind::InvalidAudioRef)?;
            let current_gen = self.generation.load(Ordering::SeqCst);
            if entry.generation != current_gen {
                return Err(AudioRefError::with_detail(
                    AudioRefErrorKind::GenerationMismatch,
                    format!(
                        "ref gen {} != current gen {}",
                        entry.generation, current_gen
                    ),
                ));
            }
            if Instant::now() > entry.expires_at {
                return Err(AudioRefError::new(AudioRefErrorKind::StaleAudioRef));
            }
            if entry.scope != scope {
                return Err(AudioRefError::with_detail(
                    AudioRefErrorKind::ScopeMismatch,
                    "scope does not match",
                ));
            }
            entry.canonical_path.clone()
        };
        // 锁外重签，避免持锁重入；issue 自身重新打开文件并复核 identity（防 TOCTOU）
        self.issue(&canonical_path, scope)
    }

    /// 当前诊断统计（测试/诊断用，不含敏感信息）。
    #[allow(dead_code)]
    pub fn stats(&self) -> RegistryStats {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        RegistryStats {
            entry_count: entries.len(),
            total_bytes: Self::total_bytes(&entries),
            generation: self.generation.load(Ordering::SeqCst),
        }
    }

    /// 清空注册表（测试用）。
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.clear();
        self.counter.store(0, Ordering::Relaxed);
        self.generation.store(0, Ordering::SeqCst);
    }
}

/// 辅助 trait——把 `Option` 转 `AudioRefError` 时携带 kind。
trait OkElse<T> {
    fn ok_else(self, kind: AudioRefErrorKind) -> Result<T, AudioRefError>;
}

impl<T> OkElse<T> for Option<T> {
    fn ok_else(self, kind: AudioRefErrorKind) -> Result<T, AudioRefError> {
        self.ok_or_else(|| AudioRefError::new(kind))
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    /// 生成临时 WAV-like bytes（无语义，只用于资源注册测试）。
    fn make_wav_bytes(size: usize) -> Vec<u8> {
        // 最小 RIFF header + 任意 data
        let mut data = vec![0u8; size];
        // 写入一些固定 magic 以确保不是全零
        if size >= 12 {
            data[..4].copy_from_slice(b"RIFF");
            data[8..12].copy_from_slice(b"WAVE");
        }
        data
    }

    /// 生成临时文件并写入内容。
    fn make_test_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    // ── token 唯一性与不可预测性 ──────────────────────────────────────────

    #[test]
    fn token_is_unique_and_unpredictable() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(1024));
        let reg = AudioResourceRegistry::default();

        let r1 = reg.issue(&path, "test").unwrap();
        let r2 = reg.issue(&path, "test").unwrap();

        // token 不同
        assert_ne!(r1, r2, "两个 token 不应相同");
        // 前缀一致
        assert!(r1.starts_with("aref_"));
        assert!(r2.starts_with("aref_"));
        // 长度一致：aref_ + 32 hex chars = 37
        assert_eq!(r1.len(), 37, "token 应为 aref_ + 32 hex chars");
        // token 不可从路径逆推
        let token1: String = r1.chars().skip(5).collect();
        assert!(token1.chars().all(|c| c.is_ascii_hexdigit()));
        // 不含路径片段（token 是纯 hex，不应包含路径分隔符）
        // token 是 hex，路径可能含 hex 字符但不应包含路径分隔符
        assert!(!token1.contains('\\'));
        assert!(!token1.contains('/'));
    }

    #[test]
    fn token_not_derived_from_path_hash() {
        let dir = tempdir().unwrap();
        // 两个内容相同但路径不同的文件
        let content = make_wav_bytes(512);
        let path1 = make_test_file(dir.path(), "a.wav", &content);
        let path2 = make_test_file(dir.path(), "b.wav", &content);

        let reg = AudioResourceRegistry::default();
        let r1 = reg.issue(&path1, "test").unwrap();
        let r2 = reg.issue(&path2, "test").unwrap();

        // 即使内容相同（hash 相同），token 也应不同
        assert_ne!(r1, r2, "相同内容不同路径的 token 应不同");
    }

    // ── TTL 过期 ──────────────────────────────────────────────────────────

    #[test]
    fn expired_ref_returns_stale() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::new(AudioResourceConfig {
            ttl: Duration::from_millis(1),
            ..Default::default()
        });

        let audio_ref = reg.issue(&path, "test").unwrap();
        // 等待过期
        std::thread::sleep(Duration::from_millis(10));

        let result = reg.resolve(&audio_ref, "test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::StaleAudioRef);
    }

    #[test]
    fn fresh_ref_resolves_successfully() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();
        let result = reg.resolve(&audio_ref, "test");
        assert!(result.is_ok(), "刚签发的 ref 应能解析");
    }

    // ── generation 隔离 ────────────────────────────────────────────────────

    #[test]
    fn generation_mismatch_rejects_old_ref() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();
        reg.bump_generation(); // 推进 generation

        let result = reg.resolve(&audio_ref, "test");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind,
            AudioRefErrorKind::GenerationMismatch
        );
    }

    #[test]
    fn current_generation_ref_works() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        reg.bump_generation(); // gen = 1
        let audio_ref = reg.issue(&path, "test").unwrap(); // gen = 1
        let result = reg.resolve(&audio_ref, "test");
        assert!(result.is_ok(), "当前 generation 的 ref 应可用");
    }

    // ── scope 隔离 ────────────────────────────────────────────────────────

    #[test]
    fn scope_mismatch_rejects() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "scope_a").unwrap();
        let result = reg.resolve(&audio_ref, "scope_b");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::ScopeMismatch);
    }

    #[test]
    fn scope_match_resolves() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "same_scope").unwrap();
        let result = reg.resolve(&audio_ref, "same_scope");
        assert!(result.is_ok(), "匹配的 scope 应能解析");
    }

    // ── clone_audio_ref ──────────────────────────────────────────────────

    #[test]
    fn clone_keeps_original_and_both_refs_resolve_once() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let original = reg.issue(&path, "test").unwrap();
        let cloned = reg.clone_audio_ref(&original, "test").unwrap();
        assert_ne!(original, cloned, "clone 应签发新 token");

        // 原 ref 未被消费：两条 ref 各自可一次性解析
        assert!(reg.resolve(&original, "test").is_ok());
        assert!(reg.resolve(&cloned, "test").is_ok());
        // 一次性授权：再次解析都应失败
        assert!(reg.resolve(&original, "test").is_err());
        assert!(reg.resolve(&cloned, "test").is_err());
    }

    #[test]
    fn clone_rejects_unknown_ref_and_scope_mismatch() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let original = reg.issue(&path, "scope_a").unwrap();
        let unknown = reg.clone_audio_ref("aref_0000000000000000", "scope_a");
        assert_eq!(
            unknown.unwrap_err().kind,
            AudioRefErrorKind::InvalidAudioRef
        );
        let mismatched = reg.clone_audio_ref(&original, "scope_b");
        assert_eq!(
            mismatched.unwrap_err().kind,
            AudioRefErrorKind::ScopeMismatch
        );
        // 失败的 clone 不消费原 ref
        assert!(reg.resolve(&original, "scope_a").is_ok());
    }

    #[test]
    fn clone_rejects_expired_ref() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let original = reg.issue(&path, "test").unwrap();
        {
            let mut entries = reg.entries.lock().unwrap();
            entries.get_mut(&original).unwrap().expires_at =
                Instant::now() - Duration::from_secs(1);
        }
        let cloned = reg.clone_audio_ref(&original, "test");
        assert_eq!(cloned.unwrap_err().kind, AudioRefErrorKind::StaleAudioRef);
    }

    // ── 容量淘汰 ──────────────────────────────────────────────────────────

    #[test]
    fn evict_when_exceeding_max_items() {
        let dir = tempdir().unwrap();
        let reg = AudioResourceRegistry::new(AudioResourceConfig {
            max_items: 3,
            ..Default::default()
        });

        let mut refs = Vec::new();
        for i in 0..3 {
            let path = make_test_file(dir.path(), &format!("file_{i}.wav"), &make_wav_bytes(64));
            refs.push(reg.issue(&path, "test").unwrap());
        }
        assert_eq!(reg.stats().entry_count, 3);

        // 第 4 项 → 淘汰最早的
        let path = make_test_file(dir.path(), "file_3.wav", &make_wav_bytes(64));
        let new_ref = reg.issue(&path, "test").unwrap();
        assert_eq!(reg.stats().entry_count, 3, "项数应保持 max_items");

        // 最早的应被淘汰
        let result = reg.resolve(&refs[0], "test");
        assert!(result.is_err());
        // 新的应在
        assert!(reg.resolve(&new_ref, "test").is_ok());
    }

    #[test]
    fn evict_when_exceeding_max_total_bytes() {
        let dir = tempdir().unwrap();
        let reg = AudioResourceRegistry::new(AudioResourceConfig {
            max_items: 100,
            max_total_bytes: 200,
            max_single_bytes: 100,
            ..Default::default()
        });

        // 3 项 64 字节 = 192 < 200
        let mut refs = Vec::new();
        for i in 0..3 {
            let path = make_test_file(dir.path(), &format!("f{i}.wav"), &make_wav_bytes(64));
            refs.push(reg.issue(&path, "test").unwrap());
        }
        assert_eq!(reg.stats().entry_count, 3);

        // 第 4 项 → 总量 256 > 200 → 淘汰
        let path = make_test_file(dir.path(), "f3.wav", &make_wav_bytes(64));
        let _ = reg.issue(&path, "test").unwrap();
        assert!(
            reg.stats().total_bytes <= 200,
            "总量应 <= 200，实际 {}",
            reg.stats().total_bytes
        );
    }

    #[test]
    fn single_file_exceeding_max_returns_error() {
        let dir = tempdir().unwrap();
        let reg = AudioResourceRegistry::new(AudioResourceConfig {
            max_single_bytes: 100,
            ..Default::default()
        });

        let path = make_test_file(dir.path(), "big.wav", &make_wav_bytes(200));
        let result = reg.issue(&path, "test");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind,
            AudioRefErrorKind::ResourceBudgetExceeded
        );
    }

    // ── 不存在 / 目录 / 跨 registry ──────────────────────────────────────

    #[test]
    fn nonexistent_file_returns_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.wav");
        let reg = AudioResourceRegistry::default();

        let result = reg.issue(&path, "test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::NotRegularFile);
    }

    #[test]
    fn directory_returns_not_regular_file() {
        let dir = tempdir().unwrap();
        let reg = AudioResourceRegistry::default();

        let result = reg.issue(dir.path(), "test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::NotRegularFile);
    }

    #[test]
    fn cross_registry_resolve_fails() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));

        let reg1 = AudioResourceRegistry::default();
        let reg2 = AudioResourceRegistry::default();

        let audio_ref = reg1.issue(&path, "test").unwrap();
        // reg2 不知道这个 ref
        let result = reg2.resolve(&audio_ref, "test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::InvalidAudioRef);
    }

    #[test]
    fn empty_ref_returns_invalid() {
        let reg = AudioResourceRegistry::default();
        let result = reg.resolve("", "test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::InvalidAudioRef);
    }

    #[test]
    fn non_aref_prefix_returns_invalid() {
        let reg = AudioResourceRegistry::default();
        let result = reg.resolve("wref_abc123", "test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::InvalidAudioRef);
    }

    // ── 内容替换（identity 变化） ─────────────────────────────────────────

    #[test]
    fn file_replaced_after_issue_fails_resolve() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();

        // 替换文件内容（size 变化）
        std::fs::write(&path, make_wav_bytes(512)).unwrap();

        let result = reg.resolve(&audio_ref, "test");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind,
            AudioRefErrorKind::FileIdentityChanged
        );
    }

    #[test]
    fn file_identity_size_change_detected() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", b"original_data_1234");
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();

        // 修改文件（写入更多内容——size 变化）
        std::fs::write(&path, b"modified_data_with_more_bytes!!!!!").unwrap();

        let result = reg.resolve(&audio_ref, "test");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind,
            AudioRefErrorKind::FileIdentityChanged
        );
    }

    #[test]
    fn same_size_in_place_change_invalidates_ref() {
        let dir = tempdir().unwrap();
        let original = make_wav_bytes(256);
        let mut changed = original.clone();
        let last = changed.last_mut().expect("wav fixture is non-empty");
        *last = last.wrapping_add(1);
        let path = make_test_file(dir.path(), "same-size.wav", &original);
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, &changed).unwrap();

        let result = reg.resolve(&audio_ref, "test");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind,
            AudioRefErrorKind::FileIdentityChanged
        );
    }

    // ── TOCTOU：打开后替换 ─────────────────────────────────────────────────

    #[test]
    fn toctou_opened_file_survives_replacement() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();
        let mut opened = reg.resolve(&audio_ref, "test").unwrap();

        // 文件已打开——即使替换文件，已打开的 handle 不受影响
        std::fs::write(&path, make_wav_bytes(512)).unwrap();

        // 已打开的 file handle 仍可使用
        use std::io::Read;
        let mut buf = [0u8; 4];
        let n = opened.file.read(&mut buf).unwrap();
        assert!(n > 0, "已打开的 file handle 应仍可读");
    }

    // ── ref 一次性授权 ─────────────────────────────────────────────────────

    #[test]
    fn resolve_is_one_shot() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();
        let _first = reg.resolve(&audio_ref, "test");
        // 第二次解析同一 ref 应失败（已从注册表移除）
        let second = reg.resolve(&audio_ref, "test");
        assert!(second.is_err());
        assert_eq!(second.unwrap_err().kind, AudioRefErrorKind::InvalidAudioRef);
    }

    // ── 符号链接策略 ───────────────────────────────────────────────────────

    #[test]
    #[cfg(target_os = "windows")]
    fn symlink_rejected_at_issue() {
        let dir = tempdir().unwrap();
        let target = make_test_file(dir.path(), "target.wav", &make_wav_bytes(256));
        let link = dir.path().join("link.wav");

        // 尝试创建 symlink（可能需要管理员权限）
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return; // 跳过：无权限
        }

        let reg = AudioResourceRegistry::default();
        let result = reg.issue(&link, "test");
        assert!(result.is_err(), "symlink 不应被签发");
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::NotRegularFile);
    }

    // ── 日志无路径 ─────────────────────────────────────────────────────────

    #[test]
    fn debug_output_has_no_absolute_path() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "test.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();

        // 错误的 Debug 不含路径
        let err = reg.resolve("aref_invalid", "test").unwrap_err();
        let debug_str = format!("{err:?}");
        assert!(
            !debug_str.contains("C:\\"),
            "Debug 输出不应含绝对路径: {debug_str}"
        );
        assert!(
            !debug_str.contains(dir.path().to_str().unwrap()),
            "Debug 输出不应含临时目录路径"
        );

        // audio_ref 本身不含路径
        assert!(!audio_ref.contains("test.wav"), "audio_ref 不应含文件名");
    }

    // ── 惰性清理 ──────────────────────────────────────────────────────────

    #[test]
    fn issue_cleans_expired_entries() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
        let reg = AudioResourceRegistry::new(AudioResourceConfig {
            ttl: Duration::from_millis(1),
            ..Default::default()
        });

        let _expired_ref = reg.issue(&path, "test").unwrap();
        std::thread::sleep(Duration::from_millis(10));

        // 签发新 ref 应触发清理旧的
        let _new_ref = reg.issue(&path, "test").unwrap();
        assert_eq!(reg.stats().entry_count, 1, "过期条目应被清理");
    }

    #[test]
    fn resolve_cleans_expired_entries() {
        let dir = tempdir().unwrap();
        let path1 = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
        let path2 = make_test_file(dir.path(), "b.wav", &make_wav_bytes(64));
        // TTL 500ms：足够在两次 issue 之间不被过期清理，又能在 sleep 后可靠过期
        let reg = AudioResourceRegistry::new(AudioResourceConfig {
            ttl: Duration::from_millis(500),
            ..Default::default()
        });

        let _ref1 = reg.issue(&path1, "test").unwrap();
        let ref2 = reg.issue(&path2, "test").unwrap();
        assert_eq!(reg.stats().entry_count, 2);

        std::thread::sleep(Duration::from_millis(600));

        // 解析 ref2 应触发清理 ref1
        let _ = reg.resolve(&ref2, "test");
        // ref2 已被移除（一次性），ref1 已过期被清理
        assert_eq!(reg.stats().entry_count, 0);
    }

    // ── 相对路径拒绝 ───────────────────────────────────────────────────────

    #[test]
    fn relative_path_rejected() {
        let reg = AudioResourceRegistry::default();
        let result = reg.issue(Path::new("relative/path.wav"), "test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, AudioRefErrorKind::NotRegularFile);
    }

    // ── OpenedAudioResource 不含路径 ────────────────────────────────────────

    #[test]
    fn opened_resource_has_no_path_field() {
        // OpenedAudioResource 不暴露路径——只有 file handle, size, identity
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
        let reg = AudioResourceRegistry::default();

        let audio_ref = reg.issue(&path, "test").unwrap();
        let opened = reg.resolve(&audio_ref, "test").unwrap();

        // 确认返回的字段不含路径
        // (OpenedAudioResource 只有 file, size, identity——编译期保证)
        assert_eq!(opened.size, 256);
        // identity 不含路径
        let id_debug = format!("{:?}", opened.identity);
        assert!(!id_debug.contains("C:\\"));
        assert!(!id_debug.contains("test.wav"));
    }
}
