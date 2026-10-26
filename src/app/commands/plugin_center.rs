//! 官方插件中心命令（0.28.5 §五.5）——目录拉取/缓存 + 从目录下载安装。
//!
//! 职责边界：
//! - **目录端点**固定为官方发布源的固定 tag Release（HTTPS），镜像候选复用
//!   既有 mirrors 工具；目录解析/静态校验在 domain `plugin::catalog`（网络零依赖）。
//! - **缓存**：内存 5 分钟 + 磁盘 catalog-cache.json 降级（端点不可达时只读
//!   回放并标记 stale——失效目录不影响已安装插件，仅"获取"视图降级）。
//! - **下载**：infra net Download 类别（代理策略生效），流式写 staging，声明
//!   bytes 同时是进度分母与写入上限（超量即中止——目录声明不容超发）；
//!   归档 sha256 由 install_official_zip 核验（单一核验点，不双读大文件）。
//! - **安装**：复用 0.28.1 状态机（暂存→校验→提交）与 0.28.2 发布编排
//!   （publish_installed_plugin），本模块只做目录解析与下载编排。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::app::command_error::CommandError;
use crate::domain::plugin::{
    self, CatalogCompat, PluginCatalog, plugin_entry_compat,
};

/// 目录内存缓存有效期：窗口内重复 get_plugin_catalog 不再发网络请求。
const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);
/// 目录拉取总时限（json 小文件，与 Update 类别同量级）。
const CATALOG_FETCH_BUDGET: Duration = Duration::from_secs(15);
/// 资产下载总时限（词库约百 MB，与托管运行时下载同预算）。
const ASSET_DOWNLOAD_BUDGET: Duration = Duration::from_secs(600);
/// 进度事件最小发射粒度（字节）——避免 IPC 洪泛。
const PROGRESS_STEP_BYTES: u64 = 512 * 1024;

/// 目录端点：官方发布仓固定 tag `plugins-catalog` 的 Release（每次插件发布
/// 覆盖更新该 Release 的资产，端点 URL 恒定）。不套用应用 updater 的
/// latest.json 语义——插件/词库与应用版本各自独立（phase §五.5）。
pub fn catalog_endpoint() -> String {
    let repo = env!("CARGO_PKG_REPOSITORY")
        .trim_start_matches("https://github.com/")
        .trim_end_matches('/');
    format!("https://github.com/{repo}/releases/download/plugins-catalog/blink-plugins-catalog.json")
}

/// 目录内存缓存（app.manage 托管）。
#[derive(Default)]
pub struct PluginCatalogState {
    inner: std::sync::RwLock<Option<CachedCatalog>>,
}

struct CachedCatalog {
    catalog: Arc<PluginCatalog>,
    fetched_at: Instant,
}

/// get_plugin_catalog 的返回（目录 + 元信息，前端渲染"获取"视图用）。
#[derive(Serialize)]
pub struct CatalogResponse {
    pub catalog: PluginCatalog,
    /// true = 端点不可达，返回的是磁盘缓存回放（展示降级标记）。
    pub stale: bool,
    pub endpoint: String,
    /// debug 构建（开发模式）才开放本地包安装入口。
    pub dev_mode: bool,
}

/// 获取官方目录（内存 TTL 内直接回；force_refresh 强制重拉）。
///
/// 失败语义：网络失败 + 无缓存 → 结构化错误（前端显示"目录不可达"与重试
/// 入口，不影响已安装插件功能）；网络失败 + 有缓存 → 缓存回放 stale=true。
#[tauri::command]
pub async fn get_plugin_catalog(
    app: tauri::AppHandle,
    force_refresh: Option<bool>,
) -> Result<CatalogResponse, CommandError> {
    let endpoint = catalog_endpoint();
    let state = app.state::<Arc<PluginCatalogState>>();
    if !force_refresh.unwrap_or(false) {
        if let Some(cached) = state.inner.read().unwrap().as_ref()
            && cached.fetched_at.elapsed() < CATALOG_TTL
        {
            return Ok(CatalogResponse {
                catalog: (*cached.catalog).clone(),
                stale: false,
                endpoint: endpoint.clone(),
                dev_mode: cfg!(debug_assertions),
            });
        }
    }

    match fetch_catalog(&endpoint).await {
        Ok(catalog) => {
            *state.inner.write().unwrap() = Some(CachedCatalog {
                catalog: Arc::new(catalog.clone()),
                fetched_at: Instant::now(),
            });
            persist_catalog_cache(&app, &endpoint);
            Ok(CatalogResponse {
                catalog,
                stale: false,
                endpoint,
                dev_mode: cfg!(debug_assertions),
            })
        }
        Err(fetch_err) => {
            tracing::warn!(%fetch_err, "插件目录拉取失败，尝试磁盘缓存回放");
            let cached = state.inner.read().unwrap().as_ref().map(|c| c.catalog.clone());
            let catalog = match cached {
                Some(c) => c,
                None => load_catalog_cache(&app).ok_or_else(|| {
                    CommandError::new(
                        "catalog_unavailable",
                        format!(
                            "插件目录不可达且无本地缓存：{fetch_err}。\
                             已安装插件不受影响；请检查网络后重试"
                        ),
                        false,
                    )
                })?,
            };
            Ok(CatalogResponse {
                catalog: (*catalog).clone(),
                stale: true,
                endpoint,
                dev_mode: cfg!(debug_assertions),
            })
        }
    }
}

/// 拉取并解析目录（主端点 + 镜像候选；任一成功即用）。
async fn fetch_catalog(endpoint: &str) -> Result<PluginCatalog, String> {
    let client = crate::infra::net::client(crate::infra::net::RequestCategory::Update);
    let candidates = crate::infra::utils::mirrors::download_candidates(endpoint);
    let mut last_err = String::from("无候选端点");
    for url in &candidates {
        let attempt = async {
            let raw = tokio::time::timeout(CATALOG_FETCH_BUDGET, client.get(url).send())
                .await
                .map_err(|_| "拉取超时".to_string())?
                .map_err(|e| e.to_string())?;
            if !raw.status().is_success() {
                return Err(format!("HTTP {}", raw.status()));
            }
            let body = raw
                .text()
                .await
                .map_err(|e| format!("读取响应失败: {e}"))?;
            plugin::parse_catalog(&body).map_err(|e| e.to_string())
        };
        match attempt.await {
            Ok(c) => {
                if url != endpoint {
                    tracing::info!(mirror = %url, "插件目录经镜像拉取成功");
                }
                return Ok(c);
            }
            Err(e) => {
                tracing::debug!(url = %url, error = %e, "插件目录候选源失败");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// 目录原文落盘（best-effort：失败只影响下次降级回放，不影响本次返回）。
fn persist_catalog_cache(app: &tauri::AppHandle, _endpoint: &str) {
    let Some(cached) = app
        .state::<Arc<PluginCatalogState>>()
        .inner
        .read()
        .unwrap()
        .as_ref()
        .map(|c| c.catalog.clone())
    else {
        return;
    };
    let storage = app.state::<Arc<plugin::PluginInstaller>>().inner();
    let path = storage.storage().catalog_cache_path();
    match serde_json::to_string_pretty(&*cached) {
        Ok(body) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, body) {
                tracing::warn!(path = %path.display(), %e, "插件目录缓存写入失败（不影响使用）");
            }
        }
        Err(e) => tracing::warn!(%e, "插件目录缓存序列化失败"),
    }
}

/// 磁盘缓存回放（端点不可达且无内存缓存时的最后降级）。
fn load_catalog_cache(app: &tauri::AppHandle) -> Option<Arc<PluginCatalog>> {
    let path = app
        .state::<Arc<plugin::PluginInstaller>>()
        .inner()
        .storage()
        .catalog_cache_path();
    let raw = std::fs::read_to_string(path).ok()?;
    plugin::parse_catalog(&raw).ok().map(Arc::new)
}

/// 从官方目录安装插件（0.28.5 主通道）。
///
/// 目录解析 → 兼容预检（宿主版本/平台，避免下载注定失败的包）→ 流式下载
/// 到 staging → install_official_zip（sha256 核验 + 状态机提交）→ 运行时
/// 发布（复用 publish_installed_plugin：同 id 替换 = 更新，旧窗口/待确认
/// 一并失效）。
#[tauri::command]
pub async fn install_plugin_from_catalog(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
) -> Result<blink_infra::plugin_storage::InstallRecord, CommandError> {
    crate::app::commands::reject_plugin_window(&window)?;
    let endpoint = catalog_endpoint();
    let state = app.state::<Arc<PluginCatalogState>>();
    let catalog = snapshot_catalog(&state, &endpoint).await?;
    let entry = catalog.plugin(&plugin_id).ok_or_else(|| {
        CommandError::new(
            "not_in_catalog",
            format!("官方目录中没有插件 {plugin_id}（目录可能已更新，请刷新后重试）"),
            false,
        )
    })?;
    precheck_compat(&entry, plugin_id.clone())?;

    emit_progress(&app, "plugin", &plugin_id, "downloading", 0, entry.asset.bytes);
    let zip_path = download_asset(&app, &entry.asset, "plugin", &plugin_id).await?;
    emit_progress(&app, "plugin", &plugin_id, "installing", entry.asset.bytes, entry.asset.bytes);

    let installer = app.state::<Arc<plugin::PluginInstaller>>().inner().clone();
    let sha = entry.asset.sha256.clone();
    let ep = endpoint.clone();
    let pid = plugin_id.clone();
    let zip_for_install = zip_path.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        installer.install_official_zip(&zip_for_install, &sha, &ep)
    })
    .await
    .map_err(|e| CommandError::new("internal", format!("安装任务失败: {e}"), true));
    // 下载暂存（含大 zip）先于错误分支清理——不等启动 recover 兜底
    cleanup_download_staging(&zip_path);
    let record = match joined {
        Ok(Ok(record)) => record,
        Ok(Err(e)) => return Err(finish_with_install_error(&app, "plugin", &pid, e)),
        Err(join_err) => return Err(join_err),
    };

    crate::app::commands::publish_installed_plugin(&app, &record).await?;
    emit_progress(&app, "plugin", &plugin_id, "done", entry.asset.bytes, entry.asset.bytes);
    Ok(record)
}

/// 从官方目录安装数据包（词库）。安装提交后插件进程按 active.json 指针
/// 自行切换（0.28.4 语义），无需运行时发布。
#[tauri::command]
pub async fn install_dataset_from_catalog(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    dataset_id: String,
) -> Result<blink_infra::plugin_storage::DatasetRecord, CommandError> {
    crate::app::commands::reject_plugin_window(&window)?;
    let endpoint = catalog_endpoint();
    let state = app.state::<Arc<PluginCatalogState>>();
    let catalog = snapshot_catalog(&state, &endpoint).await?;
    let entry = catalog.dataset(&plugin_id, &dataset_id).ok_or_else(|| {
        CommandError::new(
            "not_in_catalog",
            format!("官方目录中没有数据包 {plugin_id}/{dataset_id}（目录可能已更新，请刷新后重试）"),
            false,
        )
    })?;
    let id = format!("{plugin_id}/{dataset_id}");

    emit_progress(&app, "dataset", &id, "downloading", 0, entry.asset.bytes);
    let zip_path = download_asset(&app, &entry.asset, "dataset", &id).await?;
    emit_progress(&app, "dataset", &id, "installing", entry.asset.bytes, entry.asset.bytes);

    let installer = app
        .state::<Arc<plugin::DatasetInstaller>>()
        .inner()
        .clone();
    let sha = entry.asset.sha256.clone();
    let ep = endpoint.clone();
    let id_for_err = id.clone();
    let zip_for_install = zip_path.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        installer.install_official_zip(&zip_for_install, &sha, &ep)
    })
    .await
    .map_err(|e| CommandError::new("internal", format!("安装任务失败: {e}"), true));
    // 下载暂存（词库 zip 可达百 MB）先于错误分支清理
    cleanup_download_staging(&zip_path);
    let record = match joined {
        Ok(Ok(record)) => record,
        Ok(Err(e)) => return Err(finish_with_install_error(&app, "dataset", &id_for_err, e)),
        Err(join_err) => return Err(join_err),
    };

    emit_progress(&app, "dataset", &id, "done", entry.asset.bytes, entry.asset.bytes);
    tracing::info!(
        plugin = %record.plugin_id,
        dataset = %record.dataset_id,
        version = %record.version,
        "官方目录数据包安装完成"
    );
    Ok(record)
}

/// 取目录快照：内存缓存优先，无则现场拉取（不落盘——保持 get_plugin_catalog
/// 是唯一的磁盘缓存写入口）。
async fn snapshot_catalog(
    state: &Arc<PluginCatalogState>,
    endpoint: &str,
) -> Result<Arc<PluginCatalog>, CommandError> {
    if let Some(cached) = state.inner.read().unwrap().as_ref() {
        return Ok(cached.catalog.clone());
    }
    let catalog = fetch_catalog(endpoint).await.map_err(|e| {
        CommandError::new(
            "catalog_unavailable",
            format!("插件目录不可达：{e}。请检查网络后重试"),
            false,
        )
    })?;
    let catalog = Arc::new(catalog);
    *state.inner.write().unwrap() = Some(CachedCatalog {
        catalog: catalog.clone(),
        fetched_at: Instant::now(),
    });
    Ok(catalog)
}

/// 兼容预检（下载前快速拒绝错误架构/不兼容宿主的包）。
fn precheck_compat(
    entry: &plugin::CatalogPluginEntry,
    plugin_id: String,
) -> Result<(), CommandError> {
    match plugin_entry_compat(entry, env!("CARGO_PKG_VERSION"), plugin::current_platform_tag()) {
        CatalogCompat::Ok => Ok(()),
        CatalogCompat::HostIncompatible { min } => Err(CommandError::new(
            "host_incompatible",
            format!("插件 {plugin_id} 需要应用 ≥ {min}，请先升级 Blink"),
            false,
        )),
        CatalogCompat::PlatformMismatch { declared } => Err(CommandError::new(
            "platform_mismatch",
            format!(
                "插件 {plugin_id} 不支持当前平台（包声明 {declared:?}）"
            ),
            false,
        )),
    }
}

/// 流式下载资产到 staging（声明 bytes 是进度分母也是写入上限）。
async fn download_asset(
    app: &tauri::AppHandle,
    asset: &plugin::CatalogAsset,
    kind: &str,
    id: &str,
) -> Result<std::path::PathBuf, CommandError> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let client = crate::infra::net::client(crate::infra::net::RequestCategory::Download);
    let response = tokio::time::timeout(
        ASSET_DOWNLOAD_BUDGET,
        client.get(&asset.url).send(),
    )
    .await
    .map_err(|_| download_failed(app, kind, id, "下载超时"))?
    .map_err(|e| download_failed(app, kind, id, &format!("下载失败: {e}")))?;
    if !response.status().is_success() {
        return Err(download_failed(
            app,
            kind,
            id,
            &format!("下载失败: HTTP {}", response.status()),
        ));
    }
    if let Some(len) = response.content_length()
        && len > asset.bytes
    {
        return Err(download_failed(
            app,
            kind,
            id,
            "资产大小超过目录声明（目录可能过期，请刷新后重试）",
        ));
    }

    let storage = app.state::<Arc<plugin::PluginInstaller>>().inner();
    let staging = storage.storage().staging_dir(&format!("dl-{}", now_nanos()));
    std::fs::create_dir_all(&staging).map_err(|e| {
        CommandError::new("io", format!("创建暂存目录失败: {e}"), true)
    })?;
    let dest = staging.join("download.zip");

    let mut file = tokio::fs::File::create(&dest).await.map_err(|e| {
        CommandError::new("io", format!("创建下载文件失败: {e}"), true)
    })?;
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    let mut last_emit: u64 = 0;
    let mut write_err: Option<String> = None;
    loop {
        match stream.next().await {
            Some(Ok(bytes)) => {
                written += bytes.len() as u64;
                if written > asset.bytes {
                    write_err = Some(
                        "下载内容超过目录声明的大小（目录可能过期，请刷新后重试）".into(),
                    );
                    break;
                }
                if file.write_all(&bytes).await.is_err() {
                    write_err = Some("写入下载文件失败（磁盘空间不足？）".into());
                    break;
                }
                if written - last_emit >= PROGRESS_STEP_BYTES {
                    last_emit = written;
                    emit_progress(app, kind, id, "downloading", written, asset.bytes);
                }
            }
            Some(Err(e)) => {
                write_err = Some(format!("下载流读取失败: {e}"));
                break;
            }
            None => break,
        }
    }
    let _ = file.flush().await;
    if let Some(err) = write_err {
        let _ = tokio::fs::remove_file(&dest).await;
        let _ = std::fs::remove_dir_all(&staging);
        return Err(download_failed(app, kind, id, &err));
    }
    tracing::info!(url = %asset.url, bytes = written, "{kind} 资产下载完成");
    Ok(dest)
}

fn download_failed(
    app: &tauri::AppHandle,
    kind: &str,
    id: &str,
    reason: &str,
) -> CommandError {
    emit_progress_failed(app, kind, id, reason);
    CommandError::new("download_failed", reason.to_string(), false)
}

/// 安装失败：发 failed 事件并把 InstallError/DatasetError 折成用户可读消息。
fn finish_with_install_error<E: std::fmt::Display>(
    app: &tauri::AppHandle,
    kind: &str,
    id: &str,
    e: E,
) -> CommandError {
    let msg = e.to_string();
    emit_progress_failed(app, kind, id, &msg);
    CommandError::new("install_failed", format!("安装失败: {msg}"), false)
}

fn emit_progress(
    app: &tauri::AppHandle,
    kind: &str,
    id: &str,
    phase: &str,
    received: u64,
    total: u64,
) {
    let _ = app.emit(
        crate::infra::event_names::EventNames::PLUGIN_CENTER_PROGRESS,
        serde_json::json!({ "kind": kind, "id": id, "phase": phase, "received": received, "total": total }),
    );
}

fn emit_progress_failed(app: &tauri::AppHandle, kind: &str, id: &str, error: &str) {
    let _ = app.emit(
        crate::infra::event_names::EventNames::PLUGIN_CENTER_PROGRESS,
        serde_json::json!({ "kind": kind, "id": id, "phase": "failed", "error": error }),
    );
}

/// 安装命令结束（无论成败）后清理下载暂存目录——词库 zip 可达百 MB，
/// 不等下次启动 recover 兜底。best-effort：失败仅 debug 日志。
fn cleanup_download_staging(zip_path: &std::path::Path) {
    if let Some(dir) = zip_path.parent()
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        tracing::debug!(dir = %dir.display(), %e, "下载暂存目录清理失败（下次启动 recover 兜底）");
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
