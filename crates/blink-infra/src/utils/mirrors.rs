//! 模型/运行时下载源候选（国内可达性降级线路）。
//!
//! 三类锁定源各自的镜像策略（0.22.9 HF；0.23.19 扩展 GitHub）：
//! - **HuggingFace**（`huggingface.co`）：`BLINK_HF_ENDPOINT` 自建端点 → 主站 →
//!   hf-mirror.com 整站镜像（URL 路径同构，仅换 host）。
//! - **GitHub release 资产**（`github.com/{owner}/{repo}/releases/download/...`）：
//!   主站 → ghfast.top / gh-proxy.com / ghproxy.net 前缀式加速代理
//!   （2026-09 实测均可返回真实资产内容；社区代理，可用性随时间波动，
//!   逐个降级、全部失败才报错）。
//! - **raw.githubusercontent 文件**：主站 → cdn/fastly.jsdelivr.net
//!   （路径改写 `{owner}/{repo}@{branch}/{path}`；2026-09 实测字节一致）。
//!
//! **供应链安全**：所有下载文件的 SHA-256 在调用方编译期锁定（asset-lock /
//! gguf 锁定值）。换源只改 host/改写路径，镜像内容与锁定 hash 不一致时会被
//! 校验层拒绝，降级线路不放宽供应链约束。
//!
//! - **Python 官方发行版**（`www.python.org/ftp/python/...`）：主站 → 华为云镜像
//!   （路径同构：`mirrors.huaweicloud.com/python/...`；2026-10 实测字节一致）。
//! - **Node.js 官方发行版**（`nodejs.org/dist/...`）：主站 → 华为云镜像
//!   （路径同构：`mirrors.huaweicloud.com/nodejs/...`；2026-10 实测字节一致）。
//!
//! 非 GitHub/HF/Python/Node 主站 URL 只含原链自身，不加镜像候选。

/// HuggingFace 主站 host 前缀。
pub const HF_HOST: &str = "https://huggingface.co/";
/// HF 整站镜像（URL 路径同构，仅换 host）。
pub const HF_MIRROR_HOST: &str = "https://hf-mirror.com/";
/// 用户自定义 HF 端点（如自建镜像），置顶优先。
const HF_ENDPOINT_ENV: &str = "BLINK_HF_ENDPOINT";

/// GitHub release 资产 URL 前缀。
const GH_RELEASE_PREFIX: &str = "https://github.com/";
/// release 下载路径中段（`{owner}/{repo}/releases/download/{tag}/{file}`）。
const GH_RELEASE_MID: &str = "/releases/download/";
/// GitHub release 加速代理（前缀式：`{proxy}/{完整原链}`）。
const GH_PROXY_HOSTS: &[&str] = &[
    "https://ghfast.top",
    "https://gh-proxy.com",
    "https://ghproxy.net",
];

/// raw.githubusercontent 文件 URL 前缀。
const RAW_GH_HOST: &str = "https://raw.githubusercontent.com/";

/// Python 官方发行版 URL 前缀（0.25.20 脚本解释器托管分发）。
const PYTHON_FTPOP_HOST: &str = "https://www.python.org/ftp/python/";
/// Python 发行版华为云镜像（路径同构）。
const PYTHON_MIRROR_HOST: &str = "https://mirrors.huaweicloud.com/python/";

/// Node.js 官方发行版 URL 前缀（0.25.20 脚本解释器托管分发）。
const NODE_DIST_HOST: &str = "https://nodejs.org/dist/";
/// Node.js 发行版华为云镜像（路径同构）。
const NODE_MIRROR_HOST: &str = "https://mirrors.huaweicloud.com/nodejs/";
/// raw 文件镜像（jsDelivr，路径改写为 `/gh/{owner}/{repo}@{branch}/{path}`）。
const JSDELIVR_HOSTS: &[&str] = &["https://cdn.jsdelivr.net", "https://fastly.jsdelivr.net"];

// ── 0.28.27：国内镜像优先偏好（本机网络下载配置，进程级快照）──────────────
//
// 偏好由 app 层在启动与 `network` 配置变更时推入（`set_prefer_cn_mirror`），
// infra 侧不读配置域。切换只影响新任务——每个下载任务在规划候选时取一次
// 快照，在途任务不受影响。

static PREFER_CN_MIRROR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 设置「优先使用国内镜像下载」偏好快照（app 配置层调用）。
pub fn set_prefer_cn_mirror(prefer: bool) {
    PREFER_CN_MIRROR.store(prefer, std::sync::atomic::Ordering::Relaxed);
}

/// 读取当前偏好快照。
pub fn prefer_cn_mirror() -> bool {
    PREFER_CN_MIRROR.load(std::sync::atomic::Ordering::Relaxed)
}

/// 候选源身份（0.28.27：不靠字符串/位置推断镜像身份）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// 用户显式端点覆盖（`BLINK_HF_ENDPOINT`），恒最优先。
    EndpointOverride,
    /// 官方主源（发行站 / GitHub / raw / HF 主站）。
    Official,
    /// 已知同构镜像（华为云 / hf-mirror / jsDelivr）。
    Mirror,
    /// 加速代理（前缀式 GitHub 代理）。
    Proxy,
}

/// 带身份的下载候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadCandidate {
    pub url: String,
    pub kind: CandidateKind,
}

impl DownloadCandidate {
    fn new(url: impl Into<String>, kind: CandidateKind) -> Self {
        Self { url: url.into(), kind }
    }
}

/// 把候选列表按「镜像优先」偏好重排：开启时 Mirror/Proxy/EndpointOverride 在前、
/// Official 在后（组内保持原有顺序），关闭时 Official 在前。稳定分组重排，
/// 候选身份不丢。
pub fn order_candidates_by_preference(
    candidates: Vec<DownloadCandidate>,
    prefer_mirror: bool,
) -> Vec<DownloadCandidate> {
    if !prefer_mirror {
        return candidates;
    }
    let is_official =
        |k: &CandidateKind| matches!(k, CandidateKind::Official);
    let mut prefer_group = Vec::new();
    let mut fallback_group = Vec::new();
    for c in candidates {
        if is_official(&c.kind) {
            fallback_group.push(c);
        } else {
            prefer_group.push(c);
        }
    }
    prefer_group.extend(fallback_group);
    prefer_group
}

/// 总入口：按 URL 形态构建候选源列表（环境变量版）。
///
/// 顺序由「国内镜像优先」偏好决定：关闭 = 官方在前镜像在后（既有语义）；
/// 开启 = 镜像/加速候选在前、官方源兜底。`BLINK_HF_ENDPOINT` 显式端点恒最优先，
/// 不受偏好影响。
pub fn download_candidates(primary_url: &str) -> Vec<String> {
    download_candidates_with_preference(primary_url, prefer_cn_mirror())
        .into_iter()
        .map(|c| c.url)
        .collect()
}

/// 带身份与偏好的候选规划（0.28.27：调用方可审计每个候选的身份）。
pub fn download_candidates_with_preference(
    primary_url: &str,
    prefer_mirror: bool,
) -> Vec<DownloadCandidate> {
    let candidates = build_candidates(primary_url);
    order_candidates_by_preference(candidates, prefer_mirror)
}

/// 多资源 URL 的统一候选规划（0.28.27）：逐 URL 展开身份化候选 → URL 去重
/// （同 URL 保留首个身份）→ 按「国内镜像优先」偏好对**合并后的总列表**统一重排。
///
/// 逐 URL 各自排序不够：FFmpeg 的主源 Gyan（条目 1）必须排在 GitHub 条目
/// （条目 2）展开的加速代理之后——排序必须发生在合并之后，否则镜像优先
/// 对多源制品不生效（0.28.28 实测修复）。
pub fn combine_candidates_with_preference(
    urls: &[&str],
    prefer_mirror: bool,
) -> Vec<DownloadCandidate> {
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    for url in urls {
        for cand in build_candidates(url) {
            if seen.insert(cand.url.clone()) {
                candidates.push(cand);
            }
        }
    }
    order_candidates_by_preference(candidates, prefer_mirror)
}

/// 按 URL 形态展开身份化候选（顺序：覆盖端点 → 官方 → 镜像/代理）。
fn build_candidates(primary_url: &str) -> Vec<DownloadCandidate> {
    if primary_url.starts_with(HF_HOST) {
        return hf_candidates_with_identity(primary_url);
    }
    if is_github_release_url(primary_url) {
        let mut out = vec![DownloadCandidate::new(primary_url, CandidateKind::Official)];
        for proxy in GH_PROXY_HOSTS {
            out.push(DownloadCandidate::new(
                format!("{proxy}/{primary_url}"),
                CandidateKind::Proxy,
            ));
        }
        return out;
    }
    if primary_url.starts_with(RAW_GH_HOST) {
        return raw_github_candidates_with_identity(primary_url);
    }
    if primary_url.starts_with(PYTHON_FTPOP_HOST) {
        return mirror_host_candidates_with_identity(primary_url, PYTHON_FTPOP_HOST, PYTHON_MIRROR_HOST);
    }
    if primary_url.starts_with(NODE_DIST_HOST) {
        return mirror_host_candidates_with_identity(primary_url, NODE_DIST_HOST, NODE_MIRROR_HOST);
    }
    vec![DownloadCandidate::new(primary_url, CandidateKind::Official)]
}

/// 整站路径同构镜像候选：主站原链 → 镜像 host（0.25.20 Python/Node 发行版）。
///
/// 与 HF 镜像同型：只换 host 不改路径，内容完整性由调用方 SHA-256 锁定兜底。
fn mirror_host_candidates_with_identity(
    primary_url: &str,
    primary_host: &str,
    mirror_host: &str,
) -> Vec<DownloadCandidate> {
    let mut candidates = vec![DownloadCandidate::new(primary_url, CandidateKind::Official)];
    if let Some(path) = primary_url.strip_prefix(primary_host) {
        candidates.push(DownloadCandidate::new(
            format!("{mirror_host}{path}"),
            CandidateKind::Mirror,
        ));
    }
    candidates
}

/// HF 候选：`BLINK_HF_ENDPOINT` 覆盖 → 主站 → hf-mirror 镜像。
pub fn hf_download_candidates(primary_url: &str) -> Vec<String> {
    let endpoint = std::env::var(HF_ENDPOINT_ENV)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    hf_download_candidates_with_endpoint(primary_url, endpoint.as_deref())
}

/// HF 候选（纯函数，含偏好重排）。
pub fn hf_download_candidates_with_endpoint(
    primary_url: &str,
    endpoint: Option<&str>,
) -> Vec<String> {
    hf_candidates_with_endpoint_and_identity(primary_url, endpoint, prefer_cn_mirror())
        .into_iter()
        .map(|c| c.url)
        .collect()
}

/// HF 候选（身份版）：端点覆盖 → 主站 → hf-mirror，再按偏好重排。
pub fn hf_candidates_with_endpoint_and_identity(
    primary_url: &str,
    endpoint: Option<&str>,
    prefer_mirror: bool,
) -> Vec<DownloadCandidate> {
    let mut candidates = Vec::new();
    if let Some(ep) = endpoint
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        && let Some(path) = primary_url.strip_prefix(HF_HOST)
    {
        candidates.push(DownloadCandidate::new(
            format!("{ep}/{path}"),
            CandidateKind::EndpointOverride,
        ));
    }
    candidates.push(DownloadCandidate::new(primary_url, CandidateKind::Official));
    if let Some(path) = primary_url.strip_prefix(HF_HOST) {
        candidates.push(DownloadCandidate::new(
            format!("{HF_MIRROR_HOST}{path}"),
            CandidateKind::Mirror,
        ));
    }
    order_candidates_by_preference(candidates, prefer_mirror)
}

fn hf_candidates_with_identity(primary_url: &str) -> Vec<DownloadCandidate> {
    let endpoint = std::env::var(HF_ENDPOINT_ENV)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    hf_candidates_with_endpoint_and_identity(primary_url, endpoint.as_deref(), prefer_cn_mirror())
}

/// 判断是否为 GitHub release 资产 URL（`{GH_RELEASE_PREFIX}{owner}/{repo}/releases/download/...`）。
fn is_github_release_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix(GH_RELEASE_PREFIX) else {
        return false;
    };
    let Some(idx) = rest.find(GH_RELEASE_MID) else {
        return false;
    };
    // 中段前必须是恰好两段的 `{owner}/{repo}`（GitHub repo 名不含 '/'），
    // 两段各至少 1 字符；中段后还需有 `{tag}/{file}` 文件名
    let owner_repo = &rest[..idx];
    let mut parts = owner_repo.split('/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    !owner.is_empty()
        && !repo.is_empty()
        && parts.next().is_none()
        && rest.len() > idx + GH_RELEASE_MID.len()
}

/// GitHub 任意直链的加速代理候选（前缀式：`{proxy}/{完整原链}`）。
///
/// `download_candidates` 只识别 `releases/download/{tag}/{file}` 形态；
/// updater 的 latest.json endpoint 是 `releases/latest/download/{file}`（latest
/// 重定向形态），同属 GitHub 直链，需要相同的「主站在前、代理降级在后」候选链。
/// 签名校验只针对文件字节与下载源无关，走代理候选不放宽供应链约束。
/// 0.28.27：按偏好重排（镜像优先时代理在前）。
pub fn github_proxy_candidates(url: &str) -> Vec<String> {
    github_proxy_candidates_with_identity(url, prefer_cn_mirror())
        .into_iter()
        .map(|c| c.url)
        .collect()
}

/// GitHub 直链候选（身份版，含偏好重排）。
pub fn github_proxy_candidates_with_identity(url: &str, prefer_mirror: bool) -> Vec<DownloadCandidate> {
    let mut candidates = vec![DownloadCandidate::new(url, CandidateKind::Official)];
    for proxy in GH_PROXY_HOSTS {
        candidates.push(DownloadCandidate::new(
            format!("{proxy}/{url}"),
            CandidateKind::Proxy,
        ));
    }
    order_candidates_by_preference(candidates, prefer_mirror)
}

/// raw.githubusercontent 候选：主站 → jsDelivr（路径改写）。
fn raw_github_candidates_with_identity(primary_url: &str) -> Vec<DownloadCandidate> {
    let mut candidates = vec![DownloadCandidate::new(primary_url, CandidateKind::Official)];
    let Some(rest) = primary_url.strip_prefix(RAW_GH_HOST) else {
        return candidates;
    };
    // {owner}/{repo}/{branch}/{path...}：前三个 '/' 前的段不能为空
    let mut parts = rest.splitn(4, '/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    let branch = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() || branch.is_empty() || path.is_empty() {
        return candidates;
    }
    for host in JSDELIVR_HOSTS {
        candidates.push(DownloadCandidate::new(
            format!("{host}/gh/{owner}/{repo}@{branch}/{path}"),
            CandidateKind::Mirror,
        ));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    const PPOCR_DET: &str =
        "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx/resolve/main/inference.onnx";
    const ORT_ZIP: &str = "https://github.com/microsoft/onnxruntime/releases/download/v1.19.2/onnxruntime-win-x64-1.19.2.zip";
    const DICT: &str = "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/dict/ppocrv6_tiny_dict.txt";
    /// 0.25.20：Python/Node 发行版主站 → 华为云同构镜像，主站在前。
    #[test]
    fn python_and_node_dist_have_huaweicloud_mirror() {
        let py = download_candidates(
            "https://www.python.org/ftp/python/3.12.10/python-3.12.10-embed-amd64.zip",
        );
        assert_eq!(py[0], "https://www.python.org/ftp/python/3.12.10/python-3.12.10-embed-amd64.zip");
        assert_eq!(py[1], "https://mirrors.huaweicloud.com/python/3.12.10/python-3.12.10-embed-amd64.zip");
        assert_eq!(py.len(), 2);

        let node = download_candidates("https://nodejs.org/dist/v22.11.0/node-v22.11.0-win-x64.zip");
        assert_eq!(node[0], "https://nodejs.org/dist/v22.11.0/node-v22.11.0-win-x64.zip");
        assert_eq!(node[1], "https://mirrors.huaweicloud.com/nodejs/v22.11.0/node-v22.11.0-win-x64.zip");
        assert_eq!(node.len(), 2);
    }


    #[test]
    fn hf_url_gets_mirror_candidate() {
        let c = hf_download_candidates_with_endpoint(PPOCR_DET, None);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], PPOCR_DET);
        assert_eq!(
            c[1],
            "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_tiny_det_onnx/resolve/main/inference.onnx"
        );
    }

    #[test]
    fn endpoint_goes_first() {
        let c = hf_download_candidates_with_endpoint(
            "https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-GGUF/resolve/main/a.gguf",
            Some("https://my-hf.internal"),
        );
        assert_eq!(c.len(), 3);
        assert_eq!(
            c[0],
            "https://my-hf.internal/FunAudioLLM/Fun-ASR-Nano-GGUF/resolve/main/a.gguf"
        );
        assert!(c[2].starts_with(HF_MIRROR_HOST));
    }

    #[test]
    fn github_release_gets_proxy_candidates() {
        let c = github_proxy_candidates_with_identity(ORT_ZIP, false);
        assert_eq!(c.len(), 1 + GH_PROXY_HOSTS.len());
        assert_eq!(c[0].url, ORT_ZIP);
        assert_eq!(c[0].kind, CandidateKind::Official);
        assert_eq!(
            c[1].url,
            "https://ghfast.top/https://github.com/microsoft/onnxruntime/releases/download/v1.19.2/onnxruntime-win-x64-1.19.2.zip"
        );
        // 每个代理都是完整原链拼接
        for (proxy, cand) in GH_PROXY_HOSTS.iter().zip(c.iter().skip(1)) {
            assert!(cand.url.starts_with(&format!("{proxy}/https://github.com/")));
            assert_eq!(cand.kind, CandidateKind::Proxy);
        }
    }

    #[test]
    fn latest_json_endpoint_gets_proxy_candidates() {
        // updater 的 latest.json endpoint 是 latest 重定向形态，不在
        // download_candidates 的识别范围内，直接走 github_proxy_candidates
        let endpoint = "https://github.com/chenbihao/blink/releases/latest/download/latest.json";
        let c = github_proxy_candidates_with_identity(endpoint, false);
        assert_eq!(c.len(), 1 + GH_PROXY_HOSTS.len());
        assert_eq!(c[0].url, endpoint);
        assert_eq!(
            c[1].url,
            format!("https://ghfast.top/{endpoint}")
        );
        for (proxy, cand) in GH_PROXY_HOSTS.iter().zip(c.iter().skip(1)) {
            assert_eq!(cand.url, format!("{proxy}/{endpoint}"));
        }
    }

    #[test]
    fn raw_github_gets_jsdelivr_candidates() {
        let c = download_candidates_with_preference(DICT, false);
        assert_eq!(c.len(), 1 + JSDELIVR_HOSTS.len());
        assert_eq!(c[0].url, DICT);
        assert_eq!(c[0].kind, CandidateKind::Official);
        assert_eq!(
            c[1].url,
            "https://cdn.jsdelivr.net/gh/PaddlePaddle/PaddleOCR@main/ppocr/utils/dict/ppocrv6_tiny_dict.txt"
        );
        assert!(c[2].url.starts_with("https://fastly.jsdelivr.net/gh/"));
        assert_eq!(c[1].kind, CandidateKind::Mirror);
    }

    #[test]
    fn prefer_mirror_puts_mirror_first() {
        // 0.28.27：镜像优先 = 镜像/代理在前，官方兜底；身份保持
        let c = download_candidates_with_preference(DICT, true);
        assert_eq!(c[0].kind, CandidateKind::Mirror);
        assert_eq!(c.last().unwrap().kind, CandidateKind::Official);
        assert_eq!(c.last().unwrap().url, DICT);
        // 组内相对顺序不变
        assert!(c[0].url.starts_with("https://cdn.jsdelivr.net/"));
        assert!(c[1].url.starts_with("https://fastly.jsdelivr.net/"));

        let gh = download_candidates_with_preference(ORT_ZIP, true);
        assert_eq!(gh[0].kind, CandidateKind::Proxy);
        assert_eq!(gh.last().unwrap().url, ORT_ZIP);
        assert_eq!(gh.last().unwrap().kind, CandidateKind::Official);

        // Python/Node 同构镜像同理
        let py = download_candidates_with_preference(
            "https://www.python.org/ftp/python/3.12.10/python-3.12.10-embed-amd64.zip",
            true,
        );
        assert_eq!(py[0].kind, CandidateKind::Mirror);
        assert!(py[0].url.starts_with("https://mirrors.huaweicloud.com/python/"));
    }

    #[test]
    fn hf_endpoint_override_stays_first_even_with_mirror_preference() {
        // 显式 BLINK_HF_ENDPOINT 恒最优先，不受镜像偏好影响
        let c = hf_candidates_with_endpoint_and_identity(
            "https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-GGUF/resolve/main/a.gguf",
            Some("https://my-hf.internal"),
            true,
        );
        assert_eq!(c[0].kind, CandidateKind::EndpointOverride);
        assert_eq!(c[0].url, "https://my-hf.internal/FunAudioLLM/Fun-ASR-Nano-GGUF/resolve/main/a.gguf");
        assert_eq!(c[1].kind, CandidateKind::Mirror);
        assert_eq!(c.last().unwrap().kind, CandidateKind::Official);
    }

    #[test]
    fn combined_ffmpeg_entries_order_proxies_first_with_mirror_preference() {
        // 0.28.28 回归：FFmpeg 多条目（Gyan 主源 + GitHub fallback）合并后
        // 必须整体重排——镜像优先时代理在前、Gyan/GitHub 兜底；逐条目各自
        // 排序不会跨条目生效（实测 Gyan 排第一的缺陷）。
        let gyan = "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.7z";
        let github = "https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-essentials_build.7z";
        let combined = combine_candidates_with_preference(&[gyan, github], true);
        assert_eq!(combined.len(), 5, "去重后 1+4 个候选");
        assert!(combined[0].url.starts_with("https://ghfast.top/"), "镜像优先时代理在前");
        assert_eq!(combined[0].kind, CandidateKind::Proxy);
        assert_eq!(combined[3], DownloadCandidate::new(gyan, CandidateKind::Official));
        assert_eq!(combined[4], DownloadCandidate::new(github, CandidateKind::Official));

        // 关闭偏好：官方组在前（Gyan → GitHub），代理兜底
        let combined = combine_candidates_with_preference(&[gyan, github], false);
        assert_eq!(combined[0], DownloadCandidate::new(gyan, CandidateKind::Official));
        assert_eq!(combined[1], DownloadCandidate::new(github, CandidateKind::Official));
        assert_eq!(combined[2].kind, CandidateKind::Proxy);
    }

    #[test]
    fn unknown_url_stays_single_official_candidate() {
        let unknown = "https://example.com/foo.zip";
        let c = download_candidates_with_preference(unknown, true);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].url, unknown);
        assert_eq!(c[0].kind, CandidateKind::Official);
    }

    #[test]
    fn dispatcher_routes_by_url_shape() {
        assert_eq!(download_candidates(PPOCR_DET).len(), 2);
        assert_eq!(download_candidates(ORT_ZIP).len(), 1 + GH_PROXY_HOSTS.len());
        assert_eq!(download_candidates(DICT).len(), 1 + JSDELIVR_HOSTS.len());
        // 未知源不加镜像
        let unknown = "https://example.com/foo.zip";
        assert_eq!(download_candidates(unknown), vec![unknown.to_string()]);
        // github 但非 release 路径不加代理
        let gh_repo = "https://github.com/microsoft/onnxruntime/archive/refs/tags/v1.19.2.zip";
        assert_eq!(download_candidates(gh_repo), vec![gh_repo.to_string()]);
    }

    #[test]
    fn raw_github_malformed_path_stays_unmodified() {
        let bare = "https://raw.githubusercontent.com/only-owner";
        assert_eq!(download_candidates(bare), vec![bare.to_string()]);
    }

    #[test]
    fn github_release_malformed_path_stays_unmodified() {
        // owner 单段（无 repo）与空 owner 不加代理；最短合法形态仍加
        let owner_only = "https://github.com/onlyowner/releases/download/v1.19.2/file.zip";
        assert_eq!(download_candidates(owner_only), vec![owner_only.to_string()]);
        let empty_owner = "https://github.com//repo/releases/download/v1.19.2/file.zip";
        assert_eq!(
            download_candidates(empty_owner),
            vec![empty_owner.to_string()]
        );
        let minimal = "https://github.com/a/b/releases/download/v1/f.zip";
        assert_eq!(download_candidates(minimal).len(), 1 + GH_PROXY_HOSTS.len());
    }
}
