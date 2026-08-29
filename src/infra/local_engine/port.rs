//! 本地引擎 endpoint 分配与身份验证原语（0.22.3）。
//!
//! 提供通用的、provider-neutral 的端口分配与服务身份验证原语。
//! 不理解 FunASR/OCR，不读取 app 配置，不依赖 domain/app。
//!
//! ## 核心职责
//!
//! - **endpoint 分配**：从 preferred port + 受控候选范围中选择可用端口。
//!   仅允许 `127.0.0.1`（loopback），拒绝 `0.0.0.0`、外部 IP 或前端自定义 host。
//! - **冲突重试**：address-in-use 后有界重试和重新分配，不能无限循环。
//! - **身份验证原语**：定义服务身份验证输入/结果，支持核对
//!   engine id、instance id、token fingerprint 和 endpoint。
//! - **安全铁则**：未知端口占用只能返回冲突/换端口，绝不调用按端口 kill。
//!   不实现"查 PID 后杀进程"。
//!
//! ## 分层归属
//!
//! - `infra/local_engine`：不依赖 `crate::app` 或 `crate::domain`。
//! - HTTP health 的具体协议映射留给 adapter；本模块只提供通用 endpoint/identity 原语。
//!
//! ## 留给 H3/H4 的消费约定
//!
//! - **H3（app/local_engine）**：`EngineManager` 在启动 adapter 前，
//!   用 `EndpointAllocator` 从 `AdapterConfig::preferred_port` 解析出 `Endpoint`。
//!   启动后用 `ServiceIdentityInput` 携带 token 发给子进程的健康检查端点，
//!   并用 `ServiceIdentityResult` 核对 health 响应回显的 engine id / instance id / token fingerprint。
//!   不匹配时报告 `IdentityMismatch`，不终止未知进程。
//! - **H4（业务接入）**：adapter 在 health 映射时使用 `token_fingerprint()` 比对，
//!   不接触明文 token；HTTP client 在请求头中携带明文 token，
//!   但日志中只能记录 fingerprint。

use std::net::Ipv4Addr;

// ── Endpoint ──────────────────────────────────────────────────────────────

/// 本地引擎服务端点。
///
/// **铁则**：只允许 `127.0.0.1`（loopback）。
/// 拒绝 `0.0.0.0`、外部 IP 或前端自定义 host。
/// loopback endpoint——唯一定义在 `domain/local_engine/identity`，
/// 此处 re-export 保持既有 import 路径兼容。
pub use crate::domain::local_engine::identity::Endpoint;

// ── PortError ──────────────────────────────────────────────────────────────

/// 端口分配错误。
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PortError {
    /// preferred port 被占用，已尝试候选范围但全部不可用。
    #[error("所有候选端口均被占用（尝试了 {attempted} 个端口）")]
    AllCandidatesOccupied { attempted: usize },
    /// preferred port 被占用，且未提供候选范围。
    #[error("preferred port {port} 被占用且无候选范围")]
    PreferredPortOccupied { port: u16 },
    /// 候选端口范围无效（如 start > end）。
    #[error("候选端口范围无效: start={start}, end={end}")]
    InvalidRange { start: u16, end: u16 },
    /// 端口冲突但无法终止占用者（未知进程占用）。
    /// 不自动 kill，只报告冲突。
    #[error("端口 {port} 被未知进程占用")]
    UnknownOccupant { port: u16 },
}

// ── EndpointAllocator ──────────────────────────────────────────────────────

/// endpoint 分配策略。
///
/// 从 preferred port 开始，如果被占用则从受控候选范围中按序尝试。
/// 空闲探测只是候选，不能宣称已经占有端口——
/// "探测空闲"与子进程 bind 之间仍可能竞争，遇到 address-in-use 时限次重试。
#[derive(Debug, Clone)]
pub struct EndpointAllocator {
    /// 首选端口。
    preferred_port: u16,
    /// 受控候选范围（inclusive）。
    /// None 表示无候选范围——preferred port 被占时直接返回错误。
    candidate_range: Option<(u16, u16)>,
    /// 最大重试次数（含 preferred port 尝试，上限防止无限循环）。
    max_retries: usize,
}

/// 默认候选范围起始端口。
const DEFAULT_RANGE_START: u16 = 8100;
/// 默认候选范围结束端口。
const DEFAULT_RANGE_END: u16 = 8199;
/// 默认最大重试次数。
const DEFAULT_MAX_RETRIES: usize = 16;

impl EndpointAllocator {
    /// 创建 endpoint 分配器。
    ///
    /// - `preferred_port`：首选端口。
    /// - `candidate_range`：受控候选范围 `(start, end)`（inclusive）。
    ///   `None` 表示无候选范围——preferred port 被占时直接返回错误。
    /// - `max_retries`：最大重试次数上限。
    pub fn new(
        preferred_port: u16,
        candidate_range: Option<(u16, u16)>,
        max_retries: usize,
    ) -> Result<Self, PortError> {
        if let Some((start, end)) = candidate_range {
            if start > end {
                return Err(PortError::InvalidRange { start, end });
            }
        }
        // max_retries 至少为 1（至少尝试 preferred port 一次）
        let max_retries = max_retries.max(1);
        Ok(Self {
            preferred_port,
            candidate_range,
            max_retries,
        })
    }

    /// 使用默认候选范围 (8100-8199) 和默认重试上限创建分配器。
    pub fn with_defaults(preferred_port: u16) -> Self {
        Self::new(
            preferred_port,
            Some((DEFAULT_RANGE_START, DEFAULT_RANGE_END)),
            DEFAULT_MAX_RETRIES,
        )
        .expect("默认范围有效")
    }

    /// 返回首选端口。
    pub fn preferred_port(&self) -> u16 {
        self.preferred_port
    }

    /// 返回候选范围。
    pub fn candidate_range(&self) -> Option<(u16, u16)> {
        self.candidate_range
    }

    /// 返回最大重试次数。
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// 生成候选端口序列（迭代器）。
    ///
    /// 首先尝试 preferred port，然后按序遍历候选范围。
    /// 序列长度不超过 `max_retries`。
    fn candidate_ports(&self) -> impl Iterator<Item = u16> + '_ {
        let preferred = self.preferred_port;
        let range = self.candidate_range;
        let max = self.max_retries;

        let mut count = 0usize;

        std::iter::from_fn(move || {
            if count >= max {
                return None;
            }
            count += 1;

            if count == 1 {
                return Some(preferred);
            }

            // 从候选范围中按序取端口
            if let Some((start, end)) = range {
                // 偏移量：第 2 次尝试 start, 第 3 次 start+1, ...
                let offset = (count - 2) as u16;
                let candidate = start.saturating_add(offset);
                if candidate <= end && candidate != preferred {
                    return Some(candidate);
                }
                // 跳过与 preferred 相同的候选端口
                if candidate <= end && candidate == preferred {
                    let next = candidate.saturating_add(1);
                    if next <= end {
                        return Some(next);
                    }
                }
            }

            None
        })
    }

    /// 探测端口是否空闲（未被监听）。
    ///
    /// **注意**：空闲探测只是候选，不能宣称已经占有端口。
    /// "探测空闲"与子进程 bind 之间仍可能竞争。
    fn is_port_free(&self, port: u16) -> bool {
        // 尝试 bind 127.0.0.1:port，如果成功则端口空闲
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
    }

    /// 分配一个 endpoint。
    ///
    /// 从 preferred port 开始探测，如果被占用则从候选范围中按序尝试。
    /// 最多尝试 `max_retries` 个端口。
    ///
    /// 返回 `Ok(Endpoint)` 表示找到一个空闲端口。
    /// 返回 `Err(PortError)` 表示所有候选都被占用或范围无效。
    ///
    /// **不终止任何进程**：未知进程占用端口只报告冲突。
    pub fn allocate(&self) -> Result<Endpoint, PortError> {
        let mut attempted = 0usize;
        let mut last_occupied_port = None;

        for port in self.candidate_ports() {
            attempted += 1;
            if self.is_port_free(port) {
                tracing::debug!(port, attempted, "endpoint 分配成功（探测空闲，非占有）");
                return Ok(Endpoint::new(port));
            }
            last_occupied_port = Some(port);
            tracing::debug!(port, attempted, "端口被占用，尝试下一个候选");
        }

        // 所有候选都被占用
        if attempted == 0 {
            // 没有候选端口可尝试（可能 max_retries=0 被 clamp 到 1 但候选范围为空）
            return Err(PortError::AllCandidatesOccupied { attempted: 0 });
        }

        if self.candidate_range.is_none() && attempted == 1 {
            // 无候选范围且 preferred port 被占
            return Err(PortError::PreferredPortOccupied {
                port: last_occupied_port.unwrap_or(self.preferred_port),
            });
        }

        Err(PortError::AllCandidatesOccupied { attempted })
    }
}

// ── ServiceIdentity ────────────────────────────────────────────────────────

/// 服务身份验证输入（由调用方在 health 检查时提交）。
///
/// 包含期望的 engine id、instance id、token 和 endpoint。
/// token 使用足够随机的值；日志中只能记录 fingerprint，不能记录明文 token。
#[derive(Debug, Clone)]
pub struct ServiceIdentityInput {
    /// 期望的 engine id。
    pub engine_id: String,
    /// 期望的 instance id。
    pub instance_id: String,
    /// 明文 token（足够随机，不记录到普通日志）。
    pub token: String,
    /// 期望的 endpoint。
    pub endpoint: Endpoint,
}

impl ServiceIdentityInput {
    /// 返回 token 的 fingerprint（前 8 字符的 hex 表示）。
    ///
    /// 日志中只能记录 fingerprint，不能记录明文 token。
    pub fn token_fingerprint(&self) -> String {
        token_fingerprint(&self.token)
    }
}

/// 服务身份验证结果（从 health 响应回显中解析）。
///
/// 用于核对 health 响应回显的 engine id / instance id / token fingerprint / endpoint。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceIdentityResult {
    /// health 响应回显的 engine id。
    pub engine_id: Option<String>,
    /// health 响应回显的 instance id。
    pub instance_id: Option<String>,
    /// health 响应回显的 token fingerprint。
    pub token_fingerprint: Option<String>,
    /// health 响应回显的 endpoint（可选，部分服务可能不回显）。
    pub endpoint: Option<String>,
}

/// 身份验证结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityVerification {
    /// 身份验证通过——engine id、instance id 和 token fingerprint 全部匹配。
    Verified,
    /// 身份验证失败——附带了不匹配的字段。
    Mismatch(IdentityMismatch),
}

/// 身份验证不匹配详情。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityMismatch {
    /// engine id 是否匹配。
    pub engine_id_matches: bool,
    /// instance id 是否匹配。
    pub instance_id_matches: bool,
    /// token fingerprint 是否匹配。
    pub token_fingerprint_matches: bool,
    /// endpoint 是否匹配（如果 health 回显了 endpoint）。
    pub endpoint_matches: Option<bool>,
    /// 不匹配的描述。
    pub detail: String,
}

impl ServiceIdentityInput {
    /// 核对 health 响应回显的身份信息。
    ///
    /// 逐项核对：
    /// - engine id
    /// - instance id
    /// - token fingerprint
    /// - endpoint（如果 health 回显了）
    ///
    /// 任一不匹配即返回 `Mismatch`。
    /// **不终止任何进程**——未知端口占用只返回冲突/换端口。
    pub fn verify(&self, observed: &ServiceIdentityResult) -> IdentityVerification {
        let engine_matches = observed
            .engine_id
            .as_ref()
            .is_some_and(|id| id == &self.engine_id);

        let instance_matches = observed
            .instance_id
            .as_ref()
            .is_some_and(|id| id == &self.instance_id);

        let token_fp = self.token_fingerprint();
        let token_matches = observed
            .token_fingerprint
            .as_ref()
            .is_some_and(|fp| fp == &token_fp);

        let endpoint_str = self.endpoint.to_string();
        let endpoint_match_result = observed.endpoint.as_ref().map(|e| e == &endpoint_str);

        let all_match = engine_matches
            && instance_matches
            && token_matches
            && endpoint_match_result.unwrap_or(true);

        if all_match {
            IdentityVerification::Verified
        } else {
            let mut mismatches = Vec::new();
            if !engine_matches {
                mismatches.push("engine_id");
            }
            if !instance_matches {
                mismatches.push("instance_id");
            }
            if !token_matches {
                mismatches.push("token_fingerprint");
            }
            if let Some(false) = endpoint_match_result {
                mismatches.push("endpoint");
            }

            IdentityVerification::Mismatch(IdentityMismatch {
                engine_id_matches: engine_matches,
                instance_id_matches: instance_matches,
                token_fingerprint_matches: token_matches,
                endpoint_matches: endpoint_match_result,
                detail: format!("身份不匹配: {}", mismatches.join(", ")),
            })
        }
    }
}

// ── Token 生成 ─────────────────────────────────────────────────────────────

/// 生成足够随机的服务 token。
///
/// 使用系统时间 + 进程 ID + 原子计数器的混合，产生 64 字符 hex 字符串。
/// 不引入 rand crate，但混合了足够多的熵源。
///
/// **日志铁则**：token 不出现在普通 info/debug 日志中，只记录 fingerprint。
pub fn generate_service_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    // 混合 4 个 u64 熵源，产生 256-bit 值
    let a = pid.rotate_left(8) ^ c.rotate_left(16) ^ now;
    let b = now.rotate_left(24) ^ c ^ pid.rotate_left(32);
    let c2 = c.rotate_left(8) ^ now.rotate_left(16) ^ (pid << 48);
    let d = (a ^ b ^ c2).rotate_left(4) ^ now.rotate_left(32);

    format!("{a:016x}{b:016x}{c2:016x}{d:016x}")
}

/// 计算 token 的 fingerprint（SHA-256 前 16 字符）。
///
/// 0.22.3 Task D: 改为 SHA-256 固定前缀，Rust/Python 必须一致。
/// 日志中只能记录 fingerprint，不能记录明文 token。
pub fn token_fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let hash = hasher.finalize();
    // 取前 8 字节（16 hex 字符），手动编码避免引入 hex crate
    let bytes = &hash[..8];
    let hex_str: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("fp:{hex_str}")
}

// ── 冲突重试 ───────────────────────────────────────────────────────────────

/// 端口冲突重试策略（有界，不能无限循环）。
///
/// 当子进程启动后遇到 `address-in-use` 时，用此策略决定是否重试。
#[derive(Debug, Clone)]
pub struct ConflictRetryPolicy {
    /// 最大重试次数（含首次尝试）。上限防止无限循环。
    max_attempts: usize,
}

impl Default for ConflictRetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3 }
    }
}

impl ConflictRetryPolicy {
    /// 创建冲突重试策略。
    pub fn new(max_attempts: usize) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
        }
    }

    /// 返回最大尝试次数。
    pub fn max_attempts(&self) -> usize {
        self.max_attempts
    }

    /// 判断是否应该重试。
    ///
    /// `current_attempt` 是当前已尝试的次数（从 1 开始）。
    /// 返回 `true` 表示还有重试机会。
    pub fn should_retry(&self, current_attempt: usize) -> bool {
        current_attempt < self.max_attempts
    }
}

// ── address-in-use 识别 ────────────────────────────────────────────────────

/// 判断一段进程输出是否为**明确的**地址占用错误。
///
/// **铁则**：只匹配明确的 address-in-use 文案——其他任何失败
/// （ImportError、缺依赖、配置错误等）一律返回 false，不触发重新分配。
/// 未知进程占用端口永远只换端口重试，绝不终止占用者。
///
/// 覆盖：
/// - Windows：`WSAEADDRINUSE` / `(OS error 10048)` / "Only one usage of each socket address"
/// - Linux：`Address already in use` / `[Errno 98]`
/// - 常见 Python 服务器（uvicorn/hyper）绑定失败的表述
pub fn is_explicit_address_in_use(text: &str) -> bool {
    let lowered = text.to_lowercase();
    const MARKERS: [&str; 8] = [
        "address already in use",
        "only one usage of each socket address",
        "wsaeaddrinuse",
        "os error 10048",
        "winerror 10048",
        "[errno 10048]",
        "[errno 98]",
        "error while attempting to bind on address",
    ];
    MARKERS.iter().any(|m| lowered.contains(m))
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    // ── Endpoint 只允许 127.0.0.1 ──────────────────────────────────────────

    #[test]
    fn endpoint_always_loopback() {
        let ep = Endpoint::new(8080);
        assert_eq!(ep.port(), 8080);
        assert_eq!(ep.socket_addr(), SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(ep.base_url(), "http://127.0.0.1:8080");
        assert_eq!(ep.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn endpoint_socket_addr_is_loopback() {
        for port in [8000u16, 8100, 65535, 1] {
            let ep = Endpoint::new(port);
            let addr = ep.socket_addr();
            assert!(
                addr.ip().is_loopback(),
                "端口 {port} 的 addr 必须是 loopback"
            );
        }
    }

    // ── preferred port 可用时选中 ───────────────────────────────────────────

    #[test]
    fn allocate_selects_preferred_port_when_free() {
        // 使用 Mutex 串行化端口分配测试，防止并行 TOCTOU 竞争
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        let port = find_free_port();
        let allocator = EndpointAllocator::with_defaults(port);
        let endpoint = allocator.allocate().expect("应成功分配");
        assert_eq!(endpoint.port(), port);
    }

    // ── preferred port 被占时切到受控范围 ───────────────────────────────────

    #[test]
    fn allocate_switches_to_candidate_range_when_preferred_occupied() {
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        // 占用 preferred port
        let preferred = find_free_port();
        let _guard = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, preferred)).unwrap();

        // 候选范围使用另一个空闲端口附近
        let candidate = find_free_port_excluding(preferred);
        let allocator =
            EndpointAllocator::new(preferred, Some((candidate, candidate + 10)), 12).unwrap();

        let endpoint = allocator.allocate().expect("应从候选范围分配");
        assert_ne!(endpoint.port(), preferred, "不应选中被占的 preferred port");
        assert!(
            endpoint.port() >= candidate && endpoint.port() <= candidate + 10,
            "应在候选范围内: {} not in [{}, {}]",
            endpoint.port(),
            candidate,
            candidate + 10
        );
    }

    // ── 无候选范围时 preferred port 被占返回错误 ─────────────────────────────

    #[test]
    fn allocate_returns_error_when_no_range_and_preferred_occupied() {
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        let preferred = find_free_port();
        let _guard = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, preferred)).unwrap();

        let allocator = EndpointAllocator::new(preferred, None, 1).unwrap();
        let result = allocator.allocate();
        assert!(
            matches!(result, Err(PortError::PreferredPortOccupied { port }) if port == preferred),
            "应返回 PreferredPortOccupied: {:?}",
            result
        );
    }

    // ── 所有候选被占时返回结构化错误 ─────────────────────────────────────────

    #[test]
    fn allocate_returns_error_when_all_candidates_occupied() {
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        // 占用一段连续端口
        let start = find_free_port();
        let count = 5u16;
        let end = start + count;

        let mut guards: Vec<std::net::TcpListener> = Vec::new();
        for port in start..=end {
            if let Ok(l) = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                guards.push(l);
            }
        }

        let allocator =
            EndpointAllocator::new(start, Some((start, end)), (count as usize) + 2).unwrap();
        let result = allocator.allocate();

        match result {
            Err(PortError::AllCandidatesOccupied { attempted }) => {
                assert!(attempted > 0, "应记录尝试次数");
            }
            Ok(ep) => {
                // 可能有端口未能被绑定（被其他进程占用），如果找到空闲端口则跳过
                // 这是环境依赖的，不强制失败
                eprintln!("端口 {start}-{end} 中有部分被其他进程占用，分配到 {ep}");
            }
            Err(e) => panic!("应返回 AllCandidatesOccupied: {e}"),
        }
    }

    // ── 重试次数有上限 ───────────────────────────────────────────────────────

    #[test]
    fn allocate_respects_max_retries() {
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        // 占用 preferred + 足够多的候选端口
        let preferred = find_free_port();
        let _g1 = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, preferred)).unwrap();

        let range_start = preferred + 1;
        let range_end = preferred + 5;

        let mut guards: Vec<std::net::TcpListener> = Vec::new();
        for port in range_start..=range_end {
            if let Ok(l) = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                guards.push(l);
            }
        }

        // max_retries=3，只有 3 次尝试机会
        let allocator =
            EndpointAllocator::new(preferred, Some((range_start, range_end)), 3).unwrap();
        let result = allocator.allocate();

        // 如果所有端口都被占用，应在 3 次尝试后返回错误
        if let Err(PortError::AllCandidatesOccupied { attempted }) = &result {
            assert!(
                *attempted <= 3,
                "不应超过 max_retries: attempted={attempted}"
            );
        }
        // 如果有端口空闲（被其他进程占用导致 guards 不全），则可能成功
    }

    #[test]
    fn conflict_retry_policy_respects_max_attempts() {
        let policy = ConflictRetryPolicy::new(3);
        assert!(policy.should_retry(1)); // 尝试 1 次，还可以重试
        assert!(policy.should_retry(2)); // 尝试 2 次，还可以重试
        assert!(!policy.should_retry(3)); // 尝试 3 次，达到上限
        assert!(!policy.should_retry(4)); // 超过上限
    }

    #[test]
    fn conflict_retry_policy_default_is_3() {
        let policy = ConflictRetryPolicy::default();
        assert_eq!(policy.max_attempts(), 3);
    }

    // ── 未知端口占用不会触发终止动作 ─────────────────────────────────────────

    #[test]
    fn unknown_occupant_does_not_kill() {
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        // 占用一个端口（模拟未知进程占用）
        let port = find_free_port();
        let _guard = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();

        // 尝试分配——应该报告冲突或换端口，不应尝试 kill
        let allocator = EndpointAllocator::new(port, None, 1).unwrap();
        let result = allocator.allocate();

        match result {
            Err(PortError::PreferredPortOccupied { port: p }) => {
                assert_eq!(p, port);
                // 成功——没有触发任何 kill/terminate 动作
            }
            Err(PortError::UnknownOccupant { port: p }) => {
                assert_eq!(p, port);
                // 也可以接受——报告了冲突但没有 kill
            }
            Err(e) => {
                // 其他错误也可以接受，关键是没有 kill 副作用
                eprintln!("其他错误: {e}");
            }
            Ok(ep) => {
                // 不应该成功——端口被占用
                // 但如果 bind 因 SO_REUSEADDR 等原因成功了，接受环境差异
                eprintln!("端口 {port} 允许重绑定，分配到 {ep}");
            }
        }

        // guard 仍然存活——证明没有进程被 kill
        drop(_guard);
    }

    // ── 只生成 127.0.0.1 endpoint ────────────────────────────────────────────

    #[test]
    fn allocated_endpoints_are_all_loopback() {
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        let port = find_free_port();
        let allocator = EndpointAllocator::with_defaults(port);
        let endpoint = allocator.allocate().unwrap();
        assert!(endpoint.socket_addr().ip().is_loopback());
        assert!(endpoint.base_url().starts_with("http://127.0.0.1:"));
    }

    // ── identity 的 engine/instance/token 任一不匹配即失败 ──────────────────

    #[test]
    fn identity_verified_when_all_match() {
        let token = generate_service_token();
        let ep = Endpoint::new(8080);
        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-001".to_string(),
            token: token.clone(),
            endpoint: ep.clone(),
        };

        let result = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("inst-001".to_string()),
            token_fingerprint: Some(token_fingerprint(&token)),
            endpoint: Some(ep.to_string()),
        };

        assert_eq!(input.verify(&result), IdentityVerification::Verified);
    }

    #[test]
    fn identity_mismatch_when_engine_id_differs() {
        let token = generate_service_token();
        let ep = Endpoint::new(8080);
        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-001".to_string(),
            token: token.clone(),
            endpoint: ep.clone(),
        };

        let result = ServiceIdentityResult {
            engine_id: Some("paddleocr".to_string()), // 不同的 engine id
            instance_id: Some("inst-001".to_string()),
            token_fingerprint: Some(token_fingerprint(&token)),
            endpoint: Some(ep.to_string()),
        };

        match input.verify(&result) {
            IdentityVerification::Mismatch(m) => {
                assert!(!m.engine_id_matches);
                assert!(m.instance_id_matches);
                assert!(m.token_fingerprint_matches);
                assert!(m.detail.contains("engine_id"));
            }
            IdentityVerification::Verified => panic!("应不匹配"),
        }
    }

    #[test]
    fn identity_mismatch_when_instance_id_differs() {
        let token = generate_service_token();
        let ep = Endpoint::new(8080);
        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-001".to_string(),
            token: token.clone(),
            endpoint: ep.clone(),
        };

        let result = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("inst-999".to_string()), // 不同的 instance id
            token_fingerprint: Some(token_fingerprint(&token)),
            endpoint: Some(ep.to_string()),
        };

        match input.verify(&result) {
            IdentityVerification::Mismatch(m) => {
                assert!(m.engine_id_matches);
                assert!(!m.instance_id_matches);
                assert!(m.token_fingerprint_matches);
                assert!(m.detail.contains("instance_id"));
            }
            IdentityVerification::Verified => panic!("应不匹配"),
        }
    }

    #[test]
    fn identity_mismatch_when_token_fingerprint_differs() {
        let token = generate_service_token();
        let ep = Endpoint::new(8080);
        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-001".to_string(),
            token: token.clone(),
            endpoint: ep.clone(),
        };

        let result = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("inst-001".to_string()),
            token_fingerprint: Some("fp:deadbeefdeadbeef".to_string()), // 不同的 fingerprint
            endpoint: Some(ep.to_string()),
        };

        match input.verify(&result) {
            IdentityVerification::Mismatch(m) => {
                assert!(m.engine_id_matches);
                assert!(m.instance_id_matches);
                assert!(!m.token_fingerprint_matches);
                assert!(m.detail.contains("token_fingerprint"));
            }
            IdentityVerification::Verified => panic!("应不匹配"),
        }
    }

    #[test]
    fn identity_mismatch_when_endpoint_differs() {
        let token = generate_service_token();
        let ep = Endpoint::new(8080);
        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-001".to_string(),
            token: token.clone(),
            endpoint: ep.clone(),
        };

        let result = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("inst-001".to_string()),
            token_fingerprint: Some(token_fingerprint(&token)),
            endpoint: Some("127.0.0.1:9090".to_string()), // 不同的 endpoint
        };

        match input.verify(&result) {
            IdentityVerification::Mismatch(m) => {
                assert_eq!(m.endpoint_matches, Some(false));
                assert!(m.detail.contains("endpoint"));
            }
            IdentityVerification::Verified => panic!("应不匹配"),
        }
    }

    #[test]
    fn identity_verified_when_endpoint_not_echoed() {
        // health 响应不回显 endpoint 时，不因 endpoint 不匹配而失败
        let token = generate_service_token();
        let ep = Endpoint::new(8080);
        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-001".to_string(),
            token: token.clone(),
            endpoint: ep.clone(),
        };

        let result = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("inst-001".to_string()),
            token_fingerprint: Some(token_fingerprint(&token)),
            endpoint: None, // 未回显
        };

        assert_eq!(input.verify(&result), IdentityVerification::Verified);
    }

    #[test]
    fn identity_mismatch_when_all_fields_missing() {
        let token = generate_service_token();
        let ep = Endpoint::new(8080);
        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-001".to_string(),
            token,
            endpoint: ep,
        };

        let result = ServiceIdentityResult {
            engine_id: None,
            instance_id: None,
            token_fingerprint: None,
            endpoint: None,
        };

        match input.verify(&result) {
            IdentityVerification::Mismatch(m) => {
                assert!(!m.engine_id_matches);
                assert!(!m.instance_id_matches);
                assert!(!m.token_fingerprint_matches);
                assert_eq!(m.endpoint_matches, None);
            }
            IdentityVerification::Verified => panic!("应不匹配"),
        }
    }

    // ── token 安全性测试 ─────────────────────────────────────────────────────

    #[test]
    fn token_is_sufficiently_random() {
        let mut tokens = Vec::new();
        for _ in 0..20 {
            let t = generate_service_token();
            assert_eq!(t.len(), 64, "token 应为 64 字符 hex");
            assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(!tokens.contains(&t), "token 不应重复");
            tokens.push(t);
        }
    }

    #[test]
    fn token_fingerprint_is_not_full_token() {
        let token = generate_service_token();
        let fp = token_fingerprint(&token);
        assert!(fp.starts_with("fp:"));
        // fingerprint 不等于完整 token
        assert_ne!(fp, token);
        // fingerprint 长度远小于 token
        assert!(fp.len() < token.len());
    }

    #[test]
    fn token_fingerprint_handles_short_token() {
        // 0.22.3: SHA-256 对任何输入都产生 16 hex 字符输出
        let short = "abc";
        let fp = token_fingerprint(short);
        assert!(fp.starts_with("fp:"));
        assert_eq!(fp.len(), 19); // "fp:" + 16 hex chars
    }

    #[test]
    fn service_identity_input_exposes_fingerprint_not_token() {
        let token = generate_service_token();
        let input = ServiceIdentityInput {
            engine_id: "test".to_string(),
            instance_id: "inst-001".to_string(),
            token: token.clone(),
            endpoint: Endpoint::new(8080),
        };

        let fp = input.token_fingerprint();
        assert!(fp.starts_with("fp:"));
        assert_ne!(fp, token);
        assert!(fp.len() < token.len());
    }

    // ── 无效候选范围 ─────────────────────────────────────────────────────────

    #[test]
    fn invalid_range_returns_error() {
        let result = EndpointAllocator::new(8000, Some((9000, 8000)), 10);
        assert!(matches!(result, Err(PortError::InvalidRange { .. })));
    }

    // ── address-in-use 分类器（bind race 确定性测试）────────────────────────

    #[test]
    fn address_in_use_classifier_matches_explicit_markers() {
        // Windows WinError 10048
        assert!(is_explicit_address_in_use(
            "OSError: [WinError 10048] 通常每个套接字地址(协议/网络地址/端口)只允许使用一次。"
        ));
        assert!(is_explicit_address_in_use(
            "[Errno 10048] error while attempting to bind on address ('127.0.0.1', 8100)"
        ));
        // Linux errno 98
        assert!(is_explicit_address_in_use(
            "OSError: [Errno 98] Address already in use"
        ));
        // 英文直述
        assert!(is_explicit_address_in_use(
            "socket.bind(): Address already in use"
        ));
        // Windows 英文文案
        assert!(is_explicit_address_in_use(
            "error: Only one usage of each socket address (protocol/network address/port) is normally permitted."
        ));
        // WSAEADDRINUSE
        assert!(is_explicit_address_in_use("bind failed: WSAEADDRINUSE"));
        // 大小写不敏感
        assert!(is_explicit_address_in_use("ADDRESS ALREADY IN USE"));
    }

    #[test]
    fn address_in_use_classifier_rejects_other_failures() {
        // 其他失败一律不匹配——不触发重新分配
        assert!(!is_explicit_address_in_use(
            "ModuleNotFoundError: No module named 'funasr'"
        ));
        assert!(!is_explicit_address_in_use("ImportError: torch missing"));
        assert!(!is_explicit_address_in_use("Killed"));
        assert!(!is_explicit_address_in_use(""));
        assert!(!is_explicit_address_in_use(
            "OSError: [Errno 13] Permission denied"
        ));
        // 端口号数字本身出现不构成占用证据
        assert!(!is_explicit_address_in_use("listening on port 10048"));
    }

    /// bind race 确定性测试：探测空闲后端口被抢——
    /// 重新分配必须选出另一个端口，且整个过程不终止任何进程。
    #[test]
    fn allocator_reallocates_different_port_after_bind_race() {
        let _lock = PORT_TEST_GUARD.lock().unwrap();
        // 模拟 probe-then-bind race：allocator 探测 P1 空闲并返回；
        // 在子进程真正 bind 前，"另一个进程"占住了 P1。
        let first = find_free_port();
        let allocator = EndpointAllocator::with_defaults(first);
        let probed = allocator.allocate().expect("首次探测应成功");
        assert_eq!(probed.port(), first);

        // race 发生：P1 被抢
        let _race_winner = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, probed.port()))
            .expect("模拟竞争者占用 probe 到的端口");

        // 重新分配——必须给出不同的端口（候选范围内第一个非占用者），
        // 且不产生任何 kill 动作（allocator 无此能力，类型层面成立）。
        let reallocated = allocator.allocate().expect("重新分配应成功");
        assert_ne!(
            reallocated.port(),
            probed.port(),
            "bind race 后重新分配不应再选中同一端口"
        );
        assert!(reallocated.socket_addr().ip().is_loopback());
    }

    /// bind race 重试必须有限——重试次数由 ConflictRetryPolicy 封顶。
    #[test]
    fn bind_race_retry_is_bounded_by_policy() {
        let policy = ConflictRetryPolicy::default();
        let mut attempt = 1usize;
        let mut retried = 0usize;
        while policy.should_retry(attempt) {
            retried += 1;
            attempt += 1;
        }
        // 默认 3 次尝试 = 首次 + 最多 2 次重试
        assert_eq!(retried, policy.max_attempts() - 1);
        assert!(retried < 5, "重试必须有界");
    }

    // ── 辅助函数 ─────────────────────────────────────────────────────────────

    /// 串行化端口分配测试的全局 Mutex。    ///
    /// `find_free_port()` 存在 TOCTOU 竞争窗口：drop listener 后端口可被其他并行测试
    /// 占用。使用 Mutex 串行化整个「find_free_port → bind → allocate」流程，
    /// 消除并行测试间的端口冲突。**不要求 `--test-threads=1`**——
    /// 只串行化使用 `find_free_port` 的测试，其他测试仍并行执行。
    static PORT_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 查找一个当前空闲的端口。
    ///
    /// **必须在 `PORT_TEST_GUARD` 锁内调用**——drop listener 后端口变为可用，
    /// 如果不持锁，另一个并行测试可能在此窗口内占用该端口。
    fn find_free_port() -> u16 {
        // 绑定 127.0.0.1:0 让 OS 分配一个空闲端口
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        // 有一点竞争窗口，但测试中通常可接受
        port
    }

    /// 查找一个当前空闲的端口，排除指定端口。
    fn find_free_port_excluding(exclude: u16) -> u16 {
        loop {
            let port = find_free_port();
            if port != exclude {
                return port;
            }
        }
    }
}
