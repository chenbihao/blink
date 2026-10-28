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
#[serde(rename_all = "camelCase")]
pub struct CatalogResponse {
    pub catalog: PluginCatalog,
    /// true = 端点不可达，返回的是磁盘缓存回放（展示降级标记）。
    pub stale: bool,
    pub endpoint: String,
    /// debug 构建（开发模式）才开放本地包安装入口。
    pub dev_mode: bool,
    /// 实际目录来源（0.28.12）：`online` 在线拉取 / `cache` 官方磁盘缓存回放 /
    /// `local` dev 本地目录（显式选择或自动模式末位降级）。前端据此标注来源。
    pub source: &'static str,
    /// dev 自动回退时安装使用同一目录，避免展示本地产物却再次联网下载。
    pub local_dir: Option<String>,
}

/// dev 本地目录源（0.28.12）：`cargo xtask pack --out` 产物目录。
pub(crate) const LOCAL_CATALOG_FILE: &str = "blink-plugins-catalog.json";

/// 自动模式下本地 pack 产物的默认目录（仓库 target/plugin-dist；仅 debug 消费）。
fn default_local_pack_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("plugin-dist")
}

/// 读取本地目录源目录（0.28.12）。失败如实报错——显式本地源不静默切回线上。
fn load_local_catalog(dir: &std::path::Path) -> Result<PluginCatalog, CommandError> {
    let path = dir.join(LOCAL_CATALOG_FILE);
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        CommandError::new(
            "local_catalog_unavailable",
            format!(
                "本地目录源读取失败（{}）：{e}。请先运行 cargo xtask pack 并确认目录",
                path.display()
            ),
            false,
        )
    })?;
    plugin::parse_catalog(&raw).map_err(|e| {
        CommandError::new(
            "local_catalog_invalid",
            format!("本地目录解析失败: {e}"),
            false,
        )
    })
}

/// 本地目录源的门禁（0.28.12 生产门禁）：本地目录、文件资产读取、本地包安装
/// 全部仅 debug 构建；生产构建不消费 debug 留下的任何本地源设置/缓存。
fn reject_production_local_source() -> Result<(), CommandError> {
    if cfg!(debug_assertions) {
        Ok(())
    } else {
        Err(CommandError::new(
            "dev_only",
            "本地目录源仅开发模式可用；请使用官方插件中心",
            false,
        ))
    }
}

/// 获取官方目录（内存 TTL 内直接回；force_refresh 强制重拉）。
///
/// 失败语义：网络失败 + 无缓存 → 结构化错误（前端显示"目录不可达"与重试
/// 入口，不影响已安装插件功能）；网络失败 + 有缓存 → 缓存回放 stale=true。
///
/// `local_dir`（0.28.12，仅 debug）：显式本地目录源——优先使用，加载失败如实
/// 报错、不静默切回线上；不进内存/磁盘缓存（与官方目录分开）。
#[tauri::command]
pub async fn get_plugin_catalog(
    app: tauri::AppHandle,
    force_refresh: Option<bool>,
    local_dir: Option<String>,
) -> Result<CatalogResponse, CommandError> {
    let endpoint = catalog_endpoint();

    // 显式本地源：优先级最高，不受线上目录/磁盘缓存遮挡。
    if let Some(dir) = local_dir {
        reject_production_local_source()?;
        let dir = std::path::PathBuf::from(&dir);
        let local_dir = Some(dir.to_string_lossy().into_owned());
        let catalog = tokio::task::spawn_blocking(move || load_local_catalog(&dir))
            .await
            .map_err(|e| CommandError::new("internal", format!("本地目录读取任务失败: {e}"), true))??;
        return Ok(CatalogResponse {
            catalog,
            stale: false,
            endpoint,
            dev_mode: cfg!(debug_assertions),
            source: "local",
            local_dir,
        });
    }

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
                source: "online",
                local_dir: None,
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
                source: "online",
                local_dir: None,
            })
        }
        Err(fetch_err) => {
            tracing::warn!(%fetch_err, "插件目录拉取失败，尝试磁盘缓存回放");
            let cached = state.inner.read().unwrap().as_ref().map(|c| c.catalog.clone());
            let catalog: Option<(Arc<PluginCatalog>, &'static str)> = match cached {
                Some(c) => Some((c, "cache")),
                None => load_catalog_cache(&app)
                    .map(|c| (c, "cache"))
                    .or_else(|| {
                        // 自动模式末位降级（仅 debug）：官方在线/磁盘缓存均不可用时
                        // 回落到本地 pack 产物——断网开发期目录/安装链路仍可用。
                        if cfg!(debug_assertions) {
                            load_local_catalog(&default_local_pack_dir())
                                .ok()
                                .map(|c| (Arc::new(c), "local"))
                        } else {
                            None
                        }
                    }),
            };
            let Some((catalog, source)) = catalog else {
                return Err(CommandError::new(
                    "catalog_unavailable",
                    format!(
                        "插件目录不可达且无本地缓存：{fetch_err}。\
                         已安装插件不受影响；请检查网络后重试"
                    ),
                    false,
                ));
            };
            let stale = source != "local";
            if stale {
                tracing::info!("插件目录经磁盘缓存回放（stale）");
            } else {
                tracing::info!("插件目录降级到本地 pack 产物（开发模式）");
            }
            Ok(CatalogResponse {
                catalog: (*catalog).clone(),
                stale,
                endpoint,
                dev_mode: cfg!(debug_assertions),
                source,
                local_dir: (source == "local").then(|| default_local_pack_dir().to_string_lossy().into_owned()),
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

/// 从官方目录安装插件（0.28.5 主通道；0.28.12 增本地目录源分支）。
///
/// 目录解析 → 兼容预检（宿主版本/平台，避免下载注定失败的包）→ 流式下载
/// 到 staging → install_official_zip（sha256 核验 + 状态机提交）→ 运行时
/// 发布（复用 publish_installed_plugin：同 id 替换 = 更新，旧窗口/待确认
/// 一并失效）。
///
/// `local_dir`（仅 debug）：本地目录源——按资产文件名在 catalog 同目录定位
/// zip（pack 默认产 HTTPS URL，不改线上协议）；路径/哈希/大小校验后走
/// `install_local_catalog_zip`（LocalDev 来源，不冒充 Official）。
#[tauri::command]
pub async fn install_plugin_from_catalog(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    local_dir: Option<String>,
) -> Result<blink_infra::plugin_storage::InstallRecord, CommandError> {
    crate::app::commands::reject_plugin_window(&window)?;

    // 本地目录源分支（0.28.12）：目录快照直接重读本地文件（与展示来源一致），
    // 不经内存缓存——本地源改动（重跑 pack）后安装立即看到新目录。
    if let Some(dir) = local_dir {
        reject_production_local_source()?;
        let dir = std::path::PathBuf::from(&dir);
        let dir_for_load = dir.clone();
        let catalog = tokio::task::spawn_blocking(move || load_local_catalog(&dir_for_load))
            .await
            .map_err(|e| CommandError::new("internal", format!("本地目录读取任务失败: {e}"), true))??;
        let entry = catalog.plugin(&plugin_id).ok_or_else(|| {
            CommandError::new(
                "not_in_catalog",
                format!("本地目录中没有插件 {plugin_id}（重跑 cargo xtask pack 后重试）"),
                false,
            )
        })?;
        precheck_compat(entry, plugin_id.clone())?;

        emit_progress(&app, "plugin", &plugin_id, "installing", entry.asset.bytes, entry.asset.bytes);
        let zip_path = resolve_local_asset(&dir, &entry.asset).map_err(|e| {
            finish_with_install_error(&app, "plugin", &plugin_id, e)
        })?;
        let installer = app.state::<Arc<plugin::PluginInstaller>>().inner().clone();
        let sha = entry.asset.sha256.clone();
        let origin = dir.to_string_lossy().into_owned();
        let pid = plugin_id.clone();
        let joined = tauri::async_runtime::spawn_blocking(move || {
            installer.install_local_catalog_zip(&zip_path, &sha, &origin)
        })
        .await
        .map_err(|e| CommandError::new("internal", format!("安装任务失败: {e}"), true));
        let outcome = match joined {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => return Err(finish_with_install_error(&app, "plugin", &pid, e)),
            Err(join_err) => return Err(join_err),
        };
        crate::app::commands::publish_installed_plugin(&app, &outcome).await?;
        emit_progress(&app, "plugin", &plugin_id, "done", entry.asset.bytes, entry.asset.bytes);
        return Ok(outcome.record);
    }

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
    precheck_compat(entry, plugin_id.clone())?;

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
    let outcome = match joined {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(e)) => return Err(finish_with_install_error(&app, "plugin", &pid, e)),
        Err(join_err) => return Err(join_err),
    };

    crate::app::commands::publish_installed_plugin(&app, &outcome).await?;
    emit_progress(&app, "plugin", &plugin_id, "done", entry.asset.bytes, entry.asset.bytes);
    Ok(outcome.record)
}

/// 本地目录源的资产解析（0.28.12）：按目录条目 asset.url 的**文件名**定位
/// catalog 同目录 zip（pack 产物 URL 仍是 HTTPS 基址 + 文件名，无需改协议）。
///
/// 边界（phase 验收"篡改 zip/越界路径拒绝"）：
/// - 文件名必须是纯文件名（无路径分隔符 / `.` / `..`）；
/// - 仅接受该目录内的**常规文件**（symlink_metadata 不跟随符号链接）；
/// - canonicalize 前缀校验拒绝 junction/穿越逃逸；
/// - 实际大小必须与目录声明一致（下载侧超量中止的等价物）。
fn resolve_local_asset(
    local_dir: &std::path::Path,
    asset: &plugin::CatalogAsset,
) -> Result<std::path::PathBuf, String> {
    let filename = asset
        .url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.contains(['\\', '/'])
    {
        return Err(format!("目录条目资产文件名非法: {}", asset.url));
    }
    let path = local_dir.join(&filename);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| format!("本地资产缺失（{}）: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "本地资产不是常规文件（拒绝符号链接/目录）: {}",
            path.display()
        ));
    }
    let root = local_dir
        .canonicalize()
        .map_err(|e| format!("本地目录不可达: {e}"))?;
    let canon = path
        .canonicalize()
        .map_err(|e| format!("本地资产不可达: {e}"))?;
    if !canon.starts_with(&root) {
        return Err(format!("本地资产越界（不在目录内）: {}", canon.display()));
    }
    if meta.len() != asset.bytes {
        return Err(format!(
            "本地资产大小与目录声明不符（实际 {}，声明 {}；请重跑 cargo xtask pack）",
            meta.len(),
            asset.bytes
        ));
    }
    Ok(path)
}

/// 从官方目录安装数据包（词库）。安装提交后插件进程按 active.json 指针
/// 自行切换（0.28.4 语义），无需运行时发布。`local_dir`（0.28.12，仅 debug）
/// 为本地目录源分支，与插件安装同一来源语义。
#[tauri::command]
pub async fn install_dataset_from_catalog(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    dataset_id: String,
    local_dir: Option<String>,
) -> Result<blink_infra::plugin_storage::DatasetRecord, CommandError> {
    crate::app::commands::reject_plugin_window(&window)?;
    let id = format!("{plugin_id}/{dataset_id}");

    if let Some(dir) = local_dir {
        reject_production_local_source()?;
        let dir = std::path::PathBuf::from(&dir);
        let dir_for_load = dir.clone();
        let catalog = tokio::task::spawn_blocking(move || load_local_catalog(&dir_for_load))
            .await
            .map_err(|e| CommandError::new("internal", format!("本地目录读取任务失败: {e}"), true))??;
        let entry = catalog.dataset(&plugin_id, &dataset_id).ok_or_else(|| {
            CommandError::new(
                "not_in_catalog",
                format!("本地目录中没有数据包 {id}（重跑 cargo xtask pack 后重试）"),
                false,
            )
        })?;
        emit_progress(&app, "dataset", &id, "installing", entry.asset.bytes, entry.asset.bytes);
        let zip_path = resolve_local_asset(&dir, &entry.asset)
            .map_err(|e| finish_with_install_error(&app, "dataset", &id, e))?;
        let installer = app
            .state::<Arc<plugin::DatasetInstaller>>()
            .inner()
            .clone();
        let sha = entry.asset.sha256.clone();
        let origin = dir.to_string_lossy().into_owned();
        let id_for_err = id.clone();
        let joined = tauri::async_runtime::spawn_blocking(move || {
            installer.install_local_catalog_zip(&zip_path, &sha, &origin)
        })
        .await
        .map_err(|e| CommandError::new("internal", format!("安装任务失败: {e}"), true));
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
            "本地目录数据包安装完成"
        );
        return Ok(record);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_response_exposes_the_frontend_dev_mode_field() {
        let response = CatalogResponse {
            catalog: PluginCatalog { format_version: 1, generated_at: String::new(), plugins: vec![], datasets: vec![] },
            stale: false, endpoint: String::new(), dev_mode: false, source: "online", local_dir: None,
        };
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["devMode"], false);
        assert_eq!(json["source"], "online");
        assert!(json.get("dev_mode").is_none());
    }

    // ── 0.28.12 本地目录源资产解析边界 ─────────────────────────────────

    fn asset(url: &str, bytes: u64) -> plugin::CatalogAsset {
        plugin::CatalogAsset {
            url: url.to_string(),
            sha256: "a".repeat(64),
            bytes,
        }
    }

    #[test]
    fn resolve_local_asset_accepts_regular_file_with_matching_size() {
        let tmp = tempfile::tempdir().unwrap();
        let name = "extra.demo-1.0.0-windows-x86_64.zip";
        std::fs::write(tmp.path().join(name), vec![0u8; 16]).unwrap();
        let got = resolve_local_asset(
            tmp.path(),
            &asset(&format!("https://example.invalid/plugins-catalog/{name}"), 16),
        )
        .unwrap();
        assert_eq!(got, tmp.path().join(name));
    }

    #[test]
    fn resolve_local_asset_rejects_missing_size_mismatch_and_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let name = "x-1.0.0.zip";
        std::fs::write(tmp.path().join(name), vec![0u8; 16]).unwrap();

        // 缺失
        assert!(resolve_local_asset(tmp.path(), &asset("https://e.invalid/nope.zip", 16)).is_err());
        // 大小不符
        assert!(resolve_local_asset(tmp.path(), &asset(&format!("https://e.invalid/{name}"), 99)).is_err());
        // 文件名带路径分隔 / 空名 / 点
        assert!(resolve_local_asset(tmp.path(), &asset("https://e.invalid/a/b.zip", 16)).is_err());
        assert!(resolve_local_asset(tmp.path(), &asset("https://e.invalid/", 16)).is_err());
        assert!(resolve_local_asset(tmp.path(), &asset("https://e.invalid/..", 16)).is_err());
        // 目录不是常规文件
        std::fs::create_dir_all(tmp.path().join("dir-1.0.0.zip")).unwrap();
        assert!(resolve_local_asset(tmp.path(), &asset("https://e.invalid/dir-1.0.0.zip", 16)).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn resolve_local_asset_rejects_symlink_escape() {
        // 目录外目标经符号链接逃逸：即使 canonicalize 前缀校验兜住，也要求
        // 资产本身是常规文件（symlink_metadata 不跟随）。
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let real = outside.path().join("real-1.0.0.zip");
        std::fs::write(&real, vec![0u8; 16]).unwrap();
        let link = tmp.path().join("linked-1.0.0.zip");
        if std::os::windows::fs::symlink_file(&real, &link).is_err() {
            // 无开发者模式时跳过（符号链接创建需要权限）
            return;
        }
        assert!(resolve_local_asset(tmp.path(), &asset("https://e.invalid/linked-1.0.0.zip", 16)).is_err());
    }

    #[test]
    fn load_local_catalog_reports_structured_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // 目录缺 catalog 文件
        let err = load_local_catalog(tmp.path()).unwrap_err();
        assert_eq!(err.code, "local_catalog_unavailable");
        // 非法 JSON
        std::fs::write(tmp.path().join(LOCAL_CATALOG_FILE), "not json").unwrap();
        let err = load_local_catalog(tmp.path()).unwrap_err();
        assert_eq!(err.code, "local_catalog_invalid");
        // 合法目录
        std::fs::write(
            tmp.path().join(LOCAL_CATALOG_FILE),
            r#"{"format_version":1,"plugins":[],"datasets":[]}"#,
        )
        .unwrap();
        assert!(load_local_catalog(tmp.path()).is_ok());
    }
}
