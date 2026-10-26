//! WebDAV 客户端（0.28.6，phase §3.5「WebDAV 连接与目录」）——域内直接
//! 实现 [`SyncStore`] port。
//!
//! **落点说明**：phase 规划落点写的是 `infra/config_sync/webdav.rs`，实施时
//! 调整为 domain——SyncStore 协议类型（SyncHead/SyncError）真源在
//! `config_sync::model`，infra 反向依赖 domain 违反 §A1；而 domain 内出网
//! 已有先例（`plugin/process.rs` 的出网凭据分档），代理/重定向策略复用
//! infra `net::proxied_builder`。不产生第二份协议类型。
//!
//! - 地址规范化：用户输入 → 根 URL；若已明确指向 `blink` 结尾不重复叠加，
//!   统一在其下创建 `blink/`（集合）与 `blink/snapshots/`。同名是文件或已有
//!   不识别内容时明确报错，不覆盖（[`SyncError::RemoteConflict`]）。
//! - 认证：Basic；**非预期跨域重定向不转发认证**——同 host HTTPS 才跟随
//!   （与插件进程出网同一策略，process.rs `RedirectRule::SameHostHttps`）。
//! - 条件写：head 更新用强 ETag（`If-Match` / 首次 `If-None-Match: *`）；
//!   服务器不提供强 ETag 时由引擎按降级提示处理。
//!
//! 协议依据：RFC 4918（MKCOL 201/405、PROPFIND 207）、RFC 9110 §13.1.1。
//! 请求形状由 [`tests`] 内的本地 TCP 协议桩钉住；真实服务器兼容性归真机
//! 手测（0.28.8 记录）。

use std::time::Duration;

use base64::Engine;
use http::Method;
use serde::{Deserialize, Serialize};

use super::engine::{is_precondition_failed, SyncStore, PRECONDITION_FAILED};
use super::model::{HeadInfo, SyncHead};
use super::SyncError;

/// 请求总预算（快照是小 JSON，秒级足够；连接超时由 net 工厂注入）。
const REQUEST_BUDGET: Duration = Duration::from_secs(20);
/// 单响应体上限（防御异常服务器）。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// WebDAV 连接配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebDavConfig {
    /// 规范化后的根 URL（scheme/host/路径，结尾 `/`，指向 blink/）。
    pub root: String,
    pub username: String,
    pub password: String,
}

/// 规范化用户输入的 WebDAV 地址。
///
/// - 补默认 scheme `https://`；`http://` 显式保留（UI 提示未加密，不静默降级）。
/// - 结尾多余 `/` 统一；已以 `/blink`（任意大小写）结尾视为明确指向同步目录，
///   **不重复叠加**。
pub fn normalize_root_url(input: &str) -> Result<String, SyncError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(SyncError::NotConfigured);
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let Some((scheme, rest)) = with_scheme.split_once("://") else {
        return Err(SyncError::NotConfigured);
    };
    if scheme != "http" && scheme != "https" {
        return Err(SyncError::Network {
            message: format!("不支持的协议 {scheme}://，请使用 https:// 地址"),
        });
    }
    if !rest.contains('.') {
        return Err(SyncError::NotConfigured);
    }
    let path_trimmed = rest.trim_end_matches('/');
    let already_blink = path_trimmed
        .rsplit('/')
        .next()
        .is_some_and(|last| last.eq_ignore_ascii_case("blink"));
    let mut root = format!("{scheme}://{path_trimmed}/");
    if !already_blink {
        root.push_str("blink/");
    }
    if reqwest::Url::parse(&root).is_err() {
        return Err(SyncError::NotConfigured);
    }
    Ok(root)
}

/// WebDAV 客户端。布局：`<root>`（= blink/）下 `head.json` + `snapshots/`。
pub struct WebDavClient {
    config: WebDavConfig,
    client: reqwest::Client,
}

fn propfind() -> Method {
    Method::from_bytes(b"PROPFIND").expect("PROPFIND 是合法 method")
}

fn mkcol() -> Method {
    Method::from_bytes(b"MKCOL").expect("MKCOL 是合法 method")
}

impl WebDavClient {
    pub fn new(config: WebDavConfig) -> Result<Self, SyncError> {
        let url = reqwest::Url::parse(&config.root).map_err(|_| SyncError::NotConfigured)?;
        let host = url
            .host_str()
            .ok_or(SyncError::NotConfigured)?
            .to_string();
        // 同 host HTTPS 才跟随重定向；非预期跨域重定向不转发认证（stop）。
        let redirect = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.url().scheme() == "https"
                && attempt.url().host_str() == Some(host.as_str())
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let client = blink_infra::net::proxied_builder()
            .redirect(redirect)
            .build()
            .map_err(|e| SyncError::Network {
                message: format!("HTTP 客户端构建失败: {e}"),
            })?;
        Ok(Self { config, client })
    }

    pub fn config(&self) -> &WebDavConfig {
        &self.config
    }

    fn auth_header(&self) -> reqwest::header::HeaderValue {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.config.username, self.config.password));
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Basic {encoded}"))
            .expect("base64 是 ASCII");
        value.set_sensitive(true);
        value
    }

    fn url(&self, rel: &str) -> reqwest::Url {
        reqwest::Url::parse(&self.config.root)
            .expect("root 已在构造时校验")
            .join(rel)
            .expect("rel 是安全相对段")
    }

    async fn send(
        &self,
        method: Method,
        url: reqwest::Url,
        body: Option<Vec<u8>>,
        headers: Vec<(&'static str, String)>,
    ) -> Result<reqwest::Response, SyncError> {
        let mut request = self
            .client
            .request(method, url)
            .timeout(REQUEST_BUDGET)
            .header(reqwest::header::AUTHORIZATION, self.auth_header());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(bytes) = body {
            request = request.body(bytes);
        }
        request.send().await.map_err(map_reqwest_err)
    }

    async fn collection_exists(&self, rel: &str, what: &str) -> Result<bool, SyncError> {
        match self
            .send(propfind(), self.url(rel), None, vec![("Depth", "0".into())])
            .await
        {
            Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 207 => Ok(true),
            Ok(resp) if resp.status().as_u16() == 404 => Ok(false),
            Ok(resp) => Err(map_status(resp.status(), &format!("探测 {what}"))),
            Err(e) => Err(e),
        }
    }

    async fn mkcol(&self, rel: &str, what: &str) -> Result<(), SyncError> {
        let status = self
            .send(mkcol(), self.url(rel), None, vec![])
            .await?
            .status();
        match status.as_u16() {
            201 | 200 => Ok(()),
            // 405：目标已存在（部分服务器对已存在集合 MKCOL 返回 405）——复用。
            405 => Ok(()),
            409 => Err(SyncError::RemoteConflict {
                message: format!("创建 {what} 失败（409）：父目录不存在或被同名文件占用"),
            }),
            _ => Err(map_status(status, &format!("创建 {what}"))),
        }
    }
}

#[async_trait::async_trait]
impl SyncStore for WebDavClient {
    /// 确保布局：根集合存在（不存在则 MKCOL 创建，RFC 4918 §9.3.1 会创建
    /// 缺失中间集合）、`snapshots/` 存在。同名文件冲突报错不覆盖。
    async fn ensure_layout(&self) -> Result<(), SyncError> {
        if !self.collection_exists("", "根目录").await? {
            self.mkcol("", "blink/ 目录").await?;
        }
        if !self.collection_exists("snapshots/", "snapshots").await? {
            self.mkcol("snapshots/", "snapshots/ 目录").await?;
        }
        Ok(())
    }

    /// 读 head；None = 不存在。返回强 ETag（弱 `W/` 标签不作 CAS 依据）。
    async fn read_head(&self) -> Result<Option<HeadInfo>, SyncError> {
        let resp = self.send(Method::GET, self.url("head.json"), None, vec![]).await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(map_status(resp.status(), "读取 head"));
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .and_then(strong_etag);
        let bytes = read_bounded(resp).await?;
        let head: SyncHead = serde_json::from_slice(&bytes).map_err(|e| {
            SyncError::RemoteConflict {
                message: format!("远端 blink/head.json 不是本应用可识别格式: {e}"),
            }
        })?;
        Ok(Some(HeadInfo { head, etag }))
    }

    async fn read_snapshot(&self, name: &str) -> Result<Vec<u8>, SyncError> {
        validate_snapshot_name(name)?;
        let resp = self
            .send(Method::GET, self.url(&format!("snapshots/{name}")), None, vec![])
            .await?;
        if resp.status().as_u16() == 404 {
            return Err(SyncError::CorruptRemote {
                message: format!("快照 {name} 不存在（head 指向了缺失版本）"),
            });
        }
        if !resp.status().is_success() {
            return Err(map_status(resp.status(), "读取快照"));
        }
        read_bounded(resp).await
    }

    async fn put_snapshot(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError> {
        validate_snapshot_name(name)?;
        let resp = self
            .send(
                Method::PUT,
                self.url(&format!("snapshots/{name}")),
                Some(bytes.to_vec()),
                vec![("Content-Type", "application/json".into())],
            )
            .await?;
        if !resp.status().is_success() {
            return Err(map_status(resp.status(), "上传快照"));
        }
        Ok(())
    }

    /// 条件更新 head：`etag = None` → `If-None-Match: *`（首次创建）；
    /// 412 → message 为 [`PRECONDITION_FAILED`] 的 RemoteConflict。
    async fn put_head_conditional(
        &self,
        etag: Option<&str>,
        head: &SyncHead,
    ) -> Result<(), SyncError> {
        let body = serde_json::to_vec(head).map_err(|e| SyncError::Unknown {
            message: format!("head 序列化失败: {e}"),
        })?;
        let headers = match etag {
            Some(tag) => vec![("If-Match", tag.to_string())],
            None => vec![("If-None-Match", "*".to_string())],
        };
        let resp = self
            .send(Method::PUT, self.url("head.json"), Some(body), headers)
            .await?;
        match resp.status().as_u16() {
            412 => Err(SyncError::RemoteConflict {
                message: PRECONDITION_FAILED.to_string(),
            }),
            status if resp.status().is_success() => {
                let _ = status;
                Ok(())
            }
            _ => Err(map_status(resp.status(), "更新 head")),
        }
    }
}

/// 暴露给命令层的便捷读（测试连接时直接拿 head 状态）。
impl WebDavClient {
    pub async fn test_connection(&self) -> Result<Option<HeadInfo>, SyncError> {
        self.ensure_layout().await?;
        self.read_head().await
    }
}

/// 强 ETag 提取（`W/"..."` 弱标签不能作条件写依据，忽略）。
pub(crate) fn strong_etag(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("W/") || trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn validate_snapshot_name(name: &str) -> Result<(), SyncError> {
    let valid = !name.is_empty()
        && name.ends_with(".json")
        && !name.contains('/')
        && name
            .strip_suffix(".json")
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
    if valid {
        Ok(())
    } else {
        Err(SyncError::CorruptRemote {
            message: format!("非法快照名 {name:?}"),
        })
    }
}

fn map_status(status: reqwest::StatusCode, what: &str) -> SyncError {
    match status.as_u16() {
        401 => SyncError::Auth {
            message: format!("{what}被拒绝（401）：用户名或密码不正确"),
        },
        403 => SyncError::Permission {
            message: format!("{what}被拒绝（403）：账号可能无该目录写权限"),
        },
        404 => SyncError::Network {
            message: format!("{what}失败（404）：地址不存在，请检查 WebDAV 路径"),
        },
        409 => SyncError::RemoteConflict {
            message: format!("{what}失败（409）：远端目录冲突"),
        },
        507 => SyncError::Quota {
            message: format!("{what}失败（507）：远端空间不足"),
        },
        other => SyncError::Network {
            message: format!("{what}失败（HTTP {other}）"),
        },
    }
}

fn map_reqwest_err(e: reqwest::Error) -> SyncError {
    if e.is_timeout() {
        SyncError::Network {
            message: "连接超时".into(),
        }
    } else if e.is_connect() {
        SyncError::Network {
            message: format!("无法连接服务器: {e}"),
        }
    } else {
        SyncError::Network {
            message: e.to_string(),
        }
    }
}

async fn read_bounded(resp: reqwest::Response) -> Result<Vec<u8>, SyncError> {
    let bytes = resp.bytes().await.map_err(map_reqwest_err)?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(SyncError::CorruptRemote {
            message: format!("响应体超过 {} 上限", MAX_BODY_BYTES),
        });
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_appends_blink_once() {
        assert_eq!(
            normalize_root_url("https://dav.example.com/dav").unwrap(),
            "https://dav.example.com/dav/blink/"
        );
        // 已指向 blink 结尾：不重复叠加。
        assert_eq!(
            normalize_root_url("https://dav.example.com/dav/blink").unwrap(),
            "https://dav.example.com/dav/blink/"
        );
        assert_eq!(
            normalize_root_url("https://dav.example.com/blink/").unwrap(),
            "https://dav.example.com/blink/"
        );
        // 大小写不敏感 + 多余斜杠。
        assert_eq!(
            normalize_root_url("https://dav.example.com/dav/BLINK//").unwrap(),
            "https://dav.example.com/dav/BLINK/"
        );
        // 默认 https；显式 http 保留（UI 提示未加密）。
        assert_eq!(
            normalize_root_url("dav.example.com/dav").unwrap(),
            "https://dav.example.com/dav/blink/"
        );
        assert_eq!(
            normalize_root_url("http://192.168.1.10:5244/dav").unwrap(),
            "http://192.168.1.10:5244/dav/blink/"
        );
        assert!(normalize_root_url("").is_err());
        assert!(normalize_root_url("not a url").is_err());
        assert!(matches!(
            normalize_root_url("ftp://dav.example.com"),
            Err(SyncError::Network { .. })
        ));
    }

    #[test]
    fn strong_etag_filters_weak_tags() {
        assert_eq!(strong_etag("\"abc\""), Some("\"abc\"".into()));
        assert_eq!(strong_etag(" W/\"abc\" "), None);
        assert_eq!(strong_etag(""), None);
    }

    #[test]
    fn snapshot_name_validation() {
        assert!(validate_snapshot_name("3.json").is_ok());
        assert!(validate_snapshot_name("12.json").is_ok());
        assert!(validate_snapshot_name("").is_err());
        assert!(validate_snapshot_name("a.json").is_err());
        assert!(validate_snapshot_name("3.txt").is_err());
        assert!(validate_snapshot_name("../head.json").is_err());
        assert!(validate_snapshot_name(".json").is_err());
    }

    /// 极简 HTTP 桩：单连接单请求，返回预置响应字节。
    async fn spawn_one_shot(
        response: &'static str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            // 请求可能分多段到达：读到静默期（100ms 无新数据）为止。
            let mut raw = Vec::new();
            let mut buf = vec![0u8; 8192];
            loop {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    socket.read(&mut buf),
                )
                .await
                {
                    Ok(Ok(0)) | Err(_) => break,
                    Ok(Ok(n)) => raw.extend_from_slice(&buf[..n]),
                    Ok(Err(_)) => break,
                }
            }
            let raw = String::from_utf8_lossy(&raw).to_string();
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            vec![raw]
        });
        (addr, task)
    }

    #[tokio::test]
    async fn head_not_found_maps_to_none() {
        let (addr, task) = spawn_one_shot(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let client = WebDavClient::new(WebDavConfig {
            root: format!("http://{addr}/dav/blink/"),
            username: "u".into(),
            password: "p".into(),
        })
        .unwrap();
        assert!(client.read_head().await.unwrap().is_none());
        let raw = task.await.unwrap().remove(0);
        assert!(raw.starts_with("GET /dav/blink/head.json HTTP"));
        // hyper 输出小写头名；断言大小写不敏感。
        assert!(raw.to_ascii_lowercase().contains("authorization: basic "));
    }

    #[tokio::test]
    async fn head_with_strong_etag_roundtrips() {
        let body = r#"{"format_version":1,"revision":3,"snapshot":"3.json","sha256":"abc","device_id":"d","updated_at":1}"#;
        let response = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nETag: \"rev3\"\r\nContent-Length: {len}\r\n\r\n{body}",
                len = body.len()
            )
            .into_boxed_str(),
        );
        let (addr, task) = spawn_one_shot(response).await;
        let client = WebDavClient::new(WebDavConfig {
            root: format!("http://{addr}/blink/"),
            username: "u".into(),
            password: "p".into(),
        })
        .unwrap();
        let info = client.read_head().await.unwrap().unwrap();
        assert_eq!(info.head.revision, 3);
        assert_eq!(info.etag.as_deref(), Some("\"rev3\""));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn weak_etag_is_not_used_for_cas() {
        let body = r#"{"format_version":1,"revision":1,"snapshot":"1.json","sha256":"a","device_id":"d","updated_at":1}"#;
        let response = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nETag: W/\"weak\"\r\nContent-Length: {len}\r\n\r\n{body}",
                len = body.len()
            )
            .into_boxed_str(),
        );
        let (addr, task) = spawn_one_shot(response).await;
        let client = WebDavClient::new(WebDavConfig {
            root: format!("http://{addr}/blink/"),
            username: "u".into(),
            password: "p".into(),
        })
        .unwrap();
        let info = client.read_head().await.unwrap().unwrap();
        assert!(info.etag.is_none(), "弱 ETag 不得作为 CAS 依据");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn put_head_sends_if_match_and_maps_412() {
        // 412 分支
        let (addr, task) = spawn_one_shot(
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let client = WebDavClient::new(WebDavConfig {
            root: format!("http://{addr}/blink/"),
            username: "u".into(),
            password: "p".into(),
        })
        .unwrap();
        let head = SyncHead {
            format_version: 1,
            revision: 2,
            snapshot: "2.json".into(),
            sha256: "x".into(),
            device_id: "d".into(),
            updated_at: 2,
        };
        let err = client
            .put_head_conditional(Some("\"etag\""), &head)
            .await
            .unwrap_err();
        assert!(is_precondition_failed(&err));
        let raw = task.await.unwrap().remove(0);
        assert!(raw.to_ascii_lowercase().contains("if-match: \"etag\""));
        assert!(raw.starts_with("PUT /blink/head.json HTTP"));
    }

    #[tokio::test]
    async fn auth_failure_maps_to_auth_error() {
        let (addr, task) = spawn_one_shot(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let client = WebDavClient::new(WebDavConfig {
            root: format!("http://{addr}/blink/"),
            username: "u".into(),
            password: "wrong".into(),
        })
        .unwrap();
        let err = client.read_head().await.unwrap_err();
        assert!(matches!(err, SyncError::Auth { .. }));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn quota_maps_to_quota_error() {
        let (addr, task) = spawn_one_shot("HTTP/1.1 507 Insufficient Storage\r\nContent-Length: 0\r\n\r\n").await;
        let client = WebDavClient::new(WebDavConfig {
            root: format!("http://{addr}/blink/"),
            username: "u".into(),
            password: "p".into(),
        })
        .unwrap();
        let err = client.put_snapshot("9.json", b"{}").await.unwrap_err();
        assert!(matches!(err, SyncError::Quota { .. }));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn mkcol_409_conflict_maps_explicitly() {
        let (addr, task) = spawn_one_shot("HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\n\r\n").await;
        let client = WebDavClient::new(WebDavConfig {
            root: format!("http://{addr}/blink/"),
            username: "u".into(),
            password: "p".into(),
        })
        .unwrap();
        let err = client.mkcol("", "blink/ 目录").await.unwrap_err();
        assert!(matches!(err, SyncError::RemoteConflict { .. }));
        let raw = task.await.unwrap().remove(0);
        assert!(raw.starts_with("MKCOL /blink/ HTTP"));
    }
}
