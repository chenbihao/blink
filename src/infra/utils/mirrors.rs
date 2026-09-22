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
//! 非 GitHub/HF 主站 URL 只含原链自身，不加镜像候选。

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
/// raw 文件镜像（jsDelivr，路径改写为 `/gh/{owner}/{repo}@{branch}/{path}`）。
const JSDELIVR_HOSTS: &[&str] = &["https://cdn.jsdelivr.net", "https://fastly.jsdelivr.net"];

/// 总入口：按 URL 形态构建候选源列表（环境变量版）。
///
/// 顺序统一为「主站原链在前，镜像降级在后」——只有主站失败才逐个换源。
pub fn download_candidates(primary_url: &str) -> Vec<String> {
    if primary_url.starts_with(HF_HOST) {
        return hf_download_candidates(primary_url);
    }
    if is_github_release_url(primary_url) {
        return github_release_candidates(primary_url);
    }
    if primary_url.starts_with(RAW_GH_HOST) {
        return raw_github_candidates(primary_url);
    }
    vec![primary_url.to_string()]
}

/// HF 候选：`BLINK_HF_ENDPOINT` 覆盖 → 主站 → hf-mirror 镜像。
pub fn hf_download_candidates(primary_url: &str) -> Vec<String> {
    let endpoint = std::env::var(HF_ENDPOINT_ENV)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    hf_download_candidates_with_endpoint(primary_url, endpoint.as_deref())
}

/// HF 候选（纯函数）。
pub fn hf_download_candidates_with_endpoint(
    primary_url: &str,
    endpoint: Option<&str>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(ep) = endpoint
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        && let Some(path) = primary_url.strip_prefix(HF_HOST)
    {
        candidates.push(format!("{ep}/{path}"));
    }
    candidates.push(primary_url.to_string());
    if let Some(path) = primary_url.strip_prefix(HF_HOST) {
        candidates.push(format!("{HF_MIRROR_HOST}{path}"));
    }
    candidates
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

/// GitHub release 候选：主站 → 前缀式加速代理。
fn github_release_candidates(primary_url: &str) -> Vec<String> {
    let mut candidates = vec![primary_url.to_string()];
    for proxy in GH_PROXY_HOSTS {
        candidates.push(format!("{proxy}/{primary_url}"));
    }
    candidates
}

/// raw.githubusercontent 候选：主站 → jsDelivr（路径改写）。
fn raw_github_candidates(primary_url: &str) -> Vec<String> {
    let mut candidates = vec![primary_url.to_string()];
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
        candidates.push(format!("{host}/gh/{owner}/{repo}@{branch}/{path}"));
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
        let c = github_release_candidates(ORT_ZIP);
        assert_eq!(c.len(), 1 + GH_PROXY_HOSTS.len());
        assert_eq!(c[0], ORT_ZIP);
        assert_eq!(
            c[1],
            "https://ghfast.top/https://github.com/microsoft/onnxruntime/releases/download/v1.19.2/onnxruntime-win-x64-1.19.2.zip"
        );
        // 每个代理都是完整原链拼接
        for (proxy, cand) in GH_PROXY_HOSTS.iter().zip(c.iter().skip(1)) {
            assert!(cand.starts_with(&format!("{proxy}/https://github.com/")));
        }
    }

    #[test]
    fn raw_github_gets_jsdelivr_candidates() {
        let c = raw_github_candidates(DICT);
        assert_eq!(c.len(), 1 + JSDELIVR_HOSTS.len());
        assert_eq!(c[0], DICT);
        assert_eq!(
            c[1],
            "https://cdn.jsdelivr.net/gh/PaddlePaddle/PaddleOCR@main/ppocr/utils/dict/ppocrv6_tiny_dict.txt"
        );
        assert!(c[2].starts_with("https://fastly.jsdelivr.net/gh/"));
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
