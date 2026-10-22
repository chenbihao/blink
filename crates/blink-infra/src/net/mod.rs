//! 统一网络策略工厂（0.27.4，phase §3.5 / §8.4）。
//!
//! **职责**：管理不可变网络策略快照（HTTP/HTTPS 代理）、network generation
//! 与按请求类别复用的 `reqwest::Client`。所有出网调用点经本工厂取 client，
//! 不再各自裸建（本地 loopback 探测类除外，见 [`RequestCategory::Loopback`]）。
//!
//! **配置语义**（§8.4 定案，完全显式）：
//! - 未配置（None）= 显式直连：**忽略 `HTTP_PROXY`/`HTTPS_PROXY` 等环境变量、
//!   不读 Windows 系统代理、不做升级检测提示**；
//! - 只配某协议的代理时，另一协议直连（scheme 匹配）；
//! - loopback（`localhost` / `127.0.0.0/8` / `::1`）恒直连——用户配置的本地
//!   OpenAI/MCP endpoint 不被代理劫持；
//! - 保存前校验；无效配置保留旧快照并报错，不静默绕过代理。
//!
//! **代次与缓存失效**：`set_policy` 成功后 generation +1。Provider fingerprint
//! 与 Agent cache key 纳入 generation（`blink_domain::ai::registry`），网络变化
//! 触发两层缓存重建；在途请求持有的旧 client 是独立 Arc，不受切换影响
//! （"当前流不打断、下一轮使用新 client"）。
//!
//! **边界**：只覆盖 Blink 本体经 core client 发出的请求；插件子进程自行联网
//! 只能靠 spawn 时注入的 env 代理（`plugin::process`），不归本工厂保证。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// 代理 URL 校验/解析失败的分类错误（`set_policy` 拒绝时返回）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetPolicyError {
    #[error("代理地址无效（{field}）：{detail}")]
    Invalid { field: &'static str, detail: String },
}

/// 不可变网络策略快照——已校验的代理地址对。
///
/// `None` = 该协议显式直连。字符串形如 `http://host:port`（schema 见 §8.4）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetPolicy {
    pub http: Option<String>,
    pub https: Option<String>,
}

impl NetPolicy {
    /// 从原始输入构造并校验（空串/纯空白归一为 `None`）。
    pub fn from_raw(http: &str, https: &str) -> Result<Self, NetPolicyError> {
        let http = normalize_proxy_field("http", http)?;
        let https = normalize_proxy_field("https", https)?;
        Ok(Self { http, https })
    }

    /// 是否完全没有配置代理。
    pub fn is_direct(&self) -> bool {
        self.http.is_none() && self.https.is_none()
    }

    /// 插件子进程 env 注入用的 `(http, https)` 元组（空串 = 该协议不注入）。
    pub fn plugin_env_tuple(&self) -> (String, String) {
        (
            self.http.clone().unwrap_or_default(),
            self.https.clone().unwrap_or_default(),
        )
    }
}

/// 单个代理字段归一化 + 校验：空串 → None；否则必须是带端口的主机地址。
fn normalize_proxy_field(field: &'static str, raw: &str) -> Result<Option<String>, NetPolicyError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let url = reqwest::Url::parse(trimmed).map_err(|e| NetPolicyError::Invalid {
        field,
        detail: e.to_string(),
    })?;
    // §8.4 schema 只承诺 http 代理隧道；socks5 未启用 reqwest feature，显式拒绝
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(NetPolicyError::Invalid {
            field,
            detail: format!("不支持的协议 {}（只支持 http:// 或 https:// 代理地址）", url.scheme()),
        });
    }
    if url.host_str().is_none() {
        return Err(NetPolicyError::Invalid {
            field,
            detail: "缺少代理主机".into(),
        });
    }
    if url.port().is_none() {
        return Err(NetPolicyError::Invalid {
            field,
            detail: "缺少端口（应形如 http://127.0.0.1:7890）".into(),
        });
    }
    Ok(Some(trimmed.to_string()))
}

/// 整体策略校验（`set_policy` 的防御入口）：每个 `Some` 字段必须可再校验通过。
fn validate_policy(policy: &NetPolicy) -> Result<(), NetPolicyError> {
    for (field, value) in [("http", &policy.http), ("https", &policy.https)] {
        if let Some(raw) = value
            && normalize_proxy_field(field, raw)?.is_none()
        {
            // Some("") 之类的脏值：显式拒绝而非静默当作直连
            return Err(NetPolicyError::Invalid {
                field,
                detail: "空值应使用 null/空串表示直连".into(),
            });
        }
    }
    Ok(())
}

/// host 是否 loopback（§3.5：`localhost` / `127.0.0.0/8` / `::1` 恒直连）。
pub fn is_loopback_host(host: Option<&str>) -> bool {
    match host {
        None => true, // 无 host 的 URL 不值得经代理外发
        Some("localhost") => true,
        Some(host) => host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()),
    }
}

/// 按目标 URL 解析代理：loopback 或该协议未配置 → 直连（None）。
///
/// 这是 scheme 匹配 + loopback bypass 的唯一实现，client 构造与测试共用。
pub fn resolve_proxy_for_url(
    url: &reqwest::Url,
    policy: &NetPolicy,
) -> Option<reqwest::Url> {
    if is_loopback_host(url.host_str()) {
        return None;
    }
    let picked = match url.scheme() {
        "http" => policy.http.as_deref(),
        // https 及其他（wss 等）走 https 代理
        _ => policy.https.as_deref(),
    }?;
    reqwest::Url::parse(picked).ok()
}

// ── 请求类别 ───────────────────────────────────────────────────────────────

/// 请求类别——决定连接超时与 client 复用粒度（§3.5：不同超时/取消策略）。
///
/// 总超时不设在 client 层（流式/下载由调用方按请求设置或预算控制），
/// [`RequestCategory::Update`] 例外（检查更新天然短促且要总时限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestCategory {
    /// AI provider 请求（流式，无总超时；连接超时防黑 hole）。
    Ai,
    /// 大文件下载（GGUF/解释器/ONNX；调用方带 per-request 总超时/取消）。
    Download,
    /// 更新检查（GitHub API + 镜像 latest.json；总超时 10s）。
    Update,
    /// MCP HTTP/SSE transport（长连接，无总超时）。
    Mcp,
    /// 插件 core HTTP 代发（宿主按次代理，per-request 1-60s 超时）。
    PluginHttp,
    /// 云端 STT / 示例音频等小请求（调用方带 per-request 超时）。
    CloudStt,
}

impl RequestCategory {
    /// 连接超时——建连阶段的黑 hole 保护，不影响已建立连接。
    fn connect_timeout(self) -> Duration {
        match self {
            Self::Ai | Self::Update | Self::PluginHttp | Self::CloudStt => {
                Duration::from_secs(10)
            }
            Self::Download => Duration::from_secs(30),
            Self::Mcp => Duration::from_secs(15),
        }
    }

    /// client 层总超时——只有更新检查需要（其余由调用方控制）。
    fn total_timeout(self) -> Option<Duration> {
        match self {
            Self::Update => Some(Duration::from_secs(10)),
            _ => None,
        }
    }
}

// ── 工厂 ───────────────────────────────────────────────────────────────────

/// 网络工厂——策略快照 + generation + 按类别缓存的 client。
///
/// 并发语义：读走 `RwLock` 快照克隆，锁内只做 map 查找与 clone（廉价 Arc），
/// 不做 client 构建；`set_policy` 在写锁内整体换快照。
pub struct NetFactory {
    state: std::sync::RwLock<FactoryState>,
}

struct FactoryState {
    policy: NetPolicy,
    generation: u64,
    clients: HashMap<RequestCategory, Arc<reqwest::Client>>,
}

impl Default for NetFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl NetFactory {
    /// 空策略工厂（直连，generation 0）。测试用独立实例；生产用 [`global`]。
    pub fn new() -> Self {
        Self {
            state: std::sync::RwLock::new(FactoryState {
                policy: NetPolicy::default(),
                generation: 0,
                clients: HashMap::new(),
            }),
        }
    }

    /// 校验并切换策略快照。成功 → generation +1、client 缓存作废；
    /// 失败 → 保留旧快照与 generation（§3.5：无效配置不静默绕过代理）。
    ///
    /// 生产路径的策略来自 `from_raw`（构造期已校验）；此处二次校验防御
    /// 结构体字面量绕过 `from_raw` 的构造方式。
    pub fn set_policy(&self, policy: NetPolicy) -> Result<u64, NetPolicyError> {
        validate_policy(&policy)?;
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|e| e.into_inner());
        state.policy = policy;
        state.generation += 1;
        state.clients.clear();
        Ok(state.generation)
    }

    /// 当前策略快照。
    pub fn policy(&self) -> NetPolicy {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .clone()
    }

    /// 当前 network generation（纳入 Provider fingerprint / Agent cache key）。
    pub fn generation(&self) -> u64 {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }

    /// 取某类别的 client（同 generation 内复用同一连接池）。
    ///
    /// 未命中时在**写锁内**构建（reqwest builder 是纯 CPU，无 IO，锁窗口可接受；
    /// 保持与读路径一致的单一状态源，避免双 map 竞态）。
    pub fn client(&self, category: RequestCategory) -> reqwest::Client {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if !state.clients.contains_key(&category) {
            let client = Arc::new(build_client(&state.policy, category));
            state.clients.insert(category, client);
        }
        state
            .clients
            .get(&category)
            .expect("刚刚插入的类别必然存在")
            .as_ref()
            .clone()
    }

    /// rig transport 用的 HTTP client 包装（AI 请求共享连接池）。
    pub fn rig_http_client(&self) -> rig_reqwest::ReqwestClient {
        rig_reqwest::ReqwestClient::from(self.client(RequestCategory::Ai))
    }
}

/// 按策略构建单个类别的 client：直连显式关代理 / 配置则注入 custom proxy。
fn build_client(policy: &NetPolicy, category: RequestCategory) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(category.connect_timeout())
        .pool_idle_timeout(Duration::from_secs(90));
    if let Some(timeout) = category.total_timeout() {
        builder = builder.timeout(timeout);
    }
    if category == RequestCategory::Update {
        // 与既有更新检查行为一致（GitHub API 匿名限额按 UA 计）
        builder = builder.user_agent("blink-updater");
    }
    if policy.is_direct() {
        // §8.4：空配置 = 完全显式直连——关掉环境变量/系统代理默认探测
        builder = builder.no_proxy();
    } else {
        // 显式 custom proxy：注入任意 proxy 即关闭 reqwest 的环境变量探测；
        // scheme 匹配 + loopback bypass 由 resolve_proxy_for_url 统一裁决
        let policy = policy.clone();
        builder = builder.proxy(reqwest::Proxy::custom(move |url| {
            resolve_proxy_for_url(url, &policy)
        }));
    }
    builder.build().unwrap_or_default()
}

// ── 进程级单例 ─────────────────────────────────────────────────────────────

/// 进程级网络工厂（app 启动注入策略；与 `http_log::set_body_log_enabled`
/// 同款的进程单例模式）。domain/infra 各调用点经便捷函数访问。
static GLOBAL: std::sync::LazyLock<NetFactory> = std::sync::LazyLock::new(NetFactory::new);

/// 进程级网络工厂引用。
pub fn global() -> &'static NetFactory {
    &GLOBAL
}

/// 便捷：全局工厂当前 generation（供 fingerprint / cache key）。
pub fn generation() -> u64 {
    global().generation()
}

/// 便捷：从全局工厂取某类别 client。
pub fn client(category: RequestCategory) -> reqwest::Client {
    global().client(category)
}

/// 便捷：rig transport 的 AI client（`LoggingHttpClient` 注入点）。
pub fn rig_http_client() -> rig_reqwest::ReqwestClient {
    global().rig_http_client()
}

/// 便捷：带代理策略的 builder 起点——调用方还需自定义重定向策略等
/// client 层选项、无法走 [`client`] 缓存池时使用（如插件代发按请求分档）。
pub fn proxied_builder() -> reqwest::ClientBuilder {
    let policy = global().policy();
    let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
    if policy.is_direct() {
        builder = builder.no_proxy();
    } else {
        builder = builder.proxy(reqwest::Proxy::custom(move |url| {
            resolve_proxy_for_url(url, &policy)
        }));
    }
    builder
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_field_normalization_and_validation() {
        // 空串/空白 → None（完全直连）
        let p = NetPolicy::from_raw("", "   ").unwrap();
        assert!(p.is_direct());

        // 合法 http 代理
        let p = NetPolicy::from_raw("http://127.0.0.1:7890", "http://proxy.lan:8080").unwrap();
        assert_eq!(p.http.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(p.https.as_deref(), Some("http://proxy.lan:8080"));

        // 缺端口 / 坏协议 / 非 URL 全部拒绝
        assert!(NetPolicy::from_raw("http://127.0.0.1", "").is_err(), "缺端口");
        assert!(
            NetPolicy::from_raw("", "socks5://127.0.0.1:1080").is_err(),
            "socks 未支持，应拒绝"
        );
        assert!(NetPolicy::from_raw("not a url", "").is_err());
    }

    #[test]
    fn loopback_hosts_always_direct() {
        assert!(is_loopback_host(Some("localhost")));
        assert!(is_loopback_host(Some("127.0.0.1")));
        assert!(is_loopback_host(Some("127.255.0.3")));
        assert!(is_loopback_host(Some("::1")));
        assert!(!is_loopback_host(Some("192.168.1.2")));
        assert!(!is_loopback_host(Some("api.openai.com")));
    }

    #[test]
    fn proxy_resolves_by_scheme_and_config() {
        let policy = NetPolicy {
            http: Some("http://h-proxy:1".into()),
            https: Some("http://s-proxy:2".into()),
        };
        let https_url = reqwest::Url::parse("https://api.github.com/releases").unwrap();
        let http_url = reqwest::Url::parse("http://example.com/a").unwrap();
        let local = reqwest::Url::parse("http://127.0.0.1:8080/api").unwrap();

        assert_eq!(
            resolve_proxy_for_url(&https_url, &policy).map(|u| u.to_string()),
            Some("http://s-proxy:2/".into())
        );
        assert_eq!(
            resolve_proxy_for_url(&http_url, &policy).map(|u| u.to_string()),
            Some("http://h-proxy:1/".into())
        );
        assert_eq!(resolve_proxy_for_url(&local, &policy), None, "loopback 直连");

        // 只配 http 时 https 目标直连（协议未配置 = 直连）
        let http_only = NetPolicy {
            http: Some("http://h-proxy:1".into()),
            https: None,
        };
        assert_eq!(resolve_proxy_for_url(&https_url, &http_only), None);
        assert!(resolve_proxy_for_url(&http_url, &http_only).is_some());
    }

    #[test]
    fn factory_generation_and_client_cache_lifecycle() {
        let factory = NetFactory::new();
        assert_eq!(factory.generation(), 0);

        let c1 = factory.client(RequestCategory::Ai);
        let c2 = factory.client(RequestCategory::Ai);
        assert!(same_client(&factory, RequestCategory::Ai).is_some());
        drop((c1, c2));

        // 切换策略 → generation +1、缓存清空重建
        let new_gen = factory
            .set_policy(NetPolicy::from_raw("http://127.0.0.1:7890", "").unwrap())
            .unwrap();
        assert_eq!(new_gen, 1);
        assert_eq!(factory.generation(), 1);
        assert!(same_client(&factory, RequestCategory::Ai).is_none(), "缓存应已作废");
        let _ = factory.client(RequestCategory::Ai);
        assert!(same_client(&factory, RequestCategory::Ai).is_some());

        // 无效策略 → 拒绝且快照/generation 不变（不静默回退直连）。
        // 结构体字面量绕过 from_raw 也被 set_policy 的二次校验拦下。
        let before = factory.policy();
        let err = factory
            .set_policy(NetPolicy {
                http: Some("://bad".into()),
                https: None,
            })
            .unwrap_err();
        assert!(matches!(err, NetPolicyError::Invalid { .. }));
        assert_eq!(factory.policy(), before, "无效配置保留旧快照");
        assert_eq!(factory.generation(), 1, "无效配置不推进 generation");

        // 幂等切换：相同策略也换新代次（显式失效语义，宁重建勿漏）
        assert_eq!(
            factory
                .set_policy(NetPolicy::from_raw("http://127.0.0.1:7890", "").unwrap())
                .unwrap(),
            2
        );
    }

    /// 测试助手：当前缓存里该类别是否已有 client（None = 已作废/未建）。
    fn same_client(factory: &NetFactory, category: RequestCategory) -> Option<Arc<reqwest::Client>> {
        factory
            .state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clients
            .get(&category)
            .cloned()
    }

    #[test]
    fn proxied_builder_and_rig_client_construction() {
        // 全局工厂默认直连：builder 与 rig 包装可构建（构建不联网）
        let _ = proxied_builder().build();
        let _ = rig_http_client();
        let _ = client(RequestCategory::Download);
    }
}
