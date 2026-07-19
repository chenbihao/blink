//! 配置管理：SQLite 持久化 + 默认值 + 类型安全。
//!
//! 配置存储在 SQLite 的 `config` 表中，格式为 (key, value, updated_at)。
//! 本模块提供类型安全的配置读写，以及默认值处理。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

// ── ConfigKey trait + ConfigStore（0.8.6 §8.1.3）────────────────────────────────

/// 配置分片标识 trait（0.8.6 §8.1.3）。
///
/// 每个配置分片实现此 trait，声明自己的 KV key。
/// `ConfigStore<T>` 用 `T::KEY` 做 SQLite 存取。
///
/// 0.9 AI Provider 加 `AIConfig` 只需 `impl ConfigKey for AIConfig { const KEY = "ai.provider"; }`。
#[allow(dead_code)] // 0.9 接入时消费；当前已有 impl 但泛型调用点尚未建立
pub trait ConfigKey:
    Serialize + for<'de> Deserialize<'de> + Default + Send + Sync + 'static
{
    /// SQLite config 表的 key（如 `"app_config"` / `"app.hotkey"`）。
    const KEY: &'static str;
}

/// 泛型配置存取（0.8.6 §8.1.3）。
///
/// `ConfigStore<T>` 是无状态的——所有操作直接走 SQLite，不持连接池。
/// 调用方传 `&SqlitePool`。
#[allow(dead_code)] // 0.9 接入时消费；当前已有 impl 但泛型调用点尚未建立
pub struct ConfigStore;

impl ConfigStore {
    /// 读取配置分片。不存在或解析失败返回 `T::default()`。
    #[allow(dead_code)]
    pub async fn get<T: ConfigKey>(pool: &SqlitePool) -> T {
        crate::infra::data::history::get_config(pool, T::KEY)
            .await
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// 写入配置分片。
    #[allow(dead_code)]
    pub async fn set<T: ConfigKey>(pool: &SqlitePool, config: &T) -> Result<(), String> {
        let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
        crate::infra::data::history::set_config(pool, T::KEY, &json)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ── ConfigKey 实现 ─────────────────────────────────────────────────────────

impl ConfigKey for HotkeyConfig {
    const KEY: &'static str = "app.hotkey";
}

impl ConfigKey for AppearanceConfig {
    const KEY: &'static str = "app.appearance";
}

impl ConfigKey for SearchConfig {
    const KEY: &'static str = "app.search";
}

impl ConfigKey for SuggestionConfig {
    const KEY: &'static str = "app.suggestion";
}

impl ConfigKey for ChordConfig {
    const KEY: &'static str = "app.chord";
}

impl ConfigKey for DisableConfig {
    const KEY: &'static str = "app.disable";
}

impl ConfigKey for StartMenuConfig {
    const KEY: &'static str = "engine:start_menu";
}

impl ConfigKey for CalcConfig {
    const KEY: &'static str = "engine:calc";
}

impl ConfigKey for ContextConfig {
    const KEY: &'static str = "context:config";
}

impl ConfigKey for crate::app::stt_config::SttConfig {
    const KEY: &'static str = "stt:config";
}

impl ConfigKey for crate::infra::data::clipboard::ClipboardConfig {
    /// 0.8.8 §8.7:剪贴板配置从原 `app_config.clipboard` nested 字段独立提升为 KV,
    /// 与 6 个 AppConfig 分片同级(但不属于 `app.*` 命名空间,归到 `clipboard:*`)。
    const KEY: &'static str = "clipboard:config";
}

// 旧 `app_config` 单 key 迁移到分片后不再作为 ConfigKey；
// `AppConfig` 结构体保留为**门面**（Facade），内部 `get_config` / `save_config`
// 现在拆分到 6 片 KV（`app.hotkey` / `app.appearance` / `app.search` /
// `app.suggestion` / `app.chord` / `app.disable`）。首次读遇到旧 key 时自动迁移。
// 详见 0.8-context §8.7 + 0.8.8 收尾。

// ── 配置结构体 ──────────────────────────────────────────────────────────────────

/// 快捷键配置。
///
/// **0.8.8 分片扩展**：原只承载按键数据,现在合并 `tap_threshold` / `grace_period`
/// 两字段,让 `app.hotkey` KV 分片包含所有"热键行为"配置(对齐 phases/0.8 §8.4)。
/// 前端读 `HotkeyConfig` 时 tap/grace 走 serde default,老前端零改动。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    /// 修饰键列表（ctrl, shift, alt, meta/win）
    pub modifiers: Vec<String>,
    /// 主键（字母、数字、功能键等）
    pub key: String,
    /// 显示名称（如 "Alt+Space", "Ctrl+Shift+Space", "RightAlt"）
    pub display: String,
    /// tap 阈值(毫秒)——按下时长小于此值算 tap;超过算 hold(0.8.5 Chord 触发)
    #[serde(default = "default_tap_threshold")]
    pub tap_threshold: u64,
    /// 看门狗 grace period(毫秒)——窗口失焦后延迟隐藏的容忍时长
    #[serde(default = "default_grace_period")]
    pub grace_period: u64,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            modifiers: vec!["alt".to_string()],
            key: " ".to_string(),
            display: "Alt+Space".to_string(),
            tap_threshold: default_tap_threshold(),
            grace_period: default_grace_period(),
        }
    }
}

fn default_tap_threshold() -> u64 {
    300
}
fn default_grace_period() -> u64 {
    500
}

// ── AppConfig 分片（0.8.8 §8.7）─────────────────────────────────────────────
//
// 把原巨型 `AppConfig` 按逻辑域拆成 6 个功能分片。老结构保留为门面 struct,
// 前端与副作用命令签名零改动。`get_config` 内部改为读 6 片组装、`save_config`
// 拆分回 6 片。首次读遇到旧 `app_config` 单 key 走迁移路径,读完写回分片再删旧 key。
//
// **心智约定**:
// - 分片 struct 只承载**数据**,不放行为(save/load 走 `ConfigStore::get::<T>()`)
// - 每字段 `#[serde(default = "...")]` 防止新增字段导致老 json 反序列化失败
// - `AppConfig` 门面 struct 保留不动——`update_*` 函数继续走 `get_config → mutate → save_config`,
//   代价是每次 update 读写全部分片(可接受;IO 是 SQLite 本地),换取零业务改动
// - 0.9 前端泛型 `get_config<K>` / `set_config<K>` 接入后,`update_*` 可以逐步收敛为
//   "只写自己那片"(P1-C 完全形态,当前是过渡)

/// 外观配置分片。theme / language / auto_start + log_level(应用级设置无更合适去处,归到此片)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppearanceConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_false")]
    pub auto_start: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 主窗口透明度 (0.0 ~ 1.0)，默认 1.0 不透明
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f64,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            language: default_language(),
            auto_start: false,
            log_level: default_log_level(),
            window_opacity: default_window_opacity(),
        }
    }
}

/// 搜索行为分片。历史 / 结果数 / 分页 / surface takeover。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_true")]
    pub search_history_enabled: bool,
    #[serde(default = "default_30")]
    pub search_history_days: u32,
    #[serde(default = "default_50")]
    pub max_results: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    #[serde(default = "default_surface_takeover_enabled")]
    pub surface_takeover_enabled: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            search_history_enabled: true,
            search_history_days: 30,
            max_results: 50,
            page_size: default_page_size(),
            surface_takeover_enabled: true,
        }
    }
}

/// 建议行为分片。autosuggest + proactive。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionConfig {
    #[serde(default = "default_true")]
    pub autosuggest_enabled: bool,
    #[serde(default = "default_autosuggest_min_score")]
    pub autosuggest_min_score: f64,
    #[serde(default = "default_autosuggest_tab_key")]
    pub autosuggest_tab_key: String,
    #[serde(default = "default_false")]
    pub proactive_enabled: bool,
    #[serde(default = "default_5")]
    pub empty_query_topn: u32,
}

impl Default for SuggestionConfig {
    fn default() -> Self {
        Self {
            autosuggest_enabled: true,
            autosuggest_min_score: 0.7,
            autosuggest_tab_key: "Tab".to_string(),
            proactive_enabled: false,
            empty_query_topn: 5,
        }
    }
}

/// Chord 交互分片。总开关 + 提示可见性 + 键位绑定（0.10.7）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChordConfig {
    #[serde(default = "default_false")]
    pub chord_enabled: bool,
    #[serde(default = "default_true")]
    pub chord_hint_visible: bool,
    /// 0.10.7：chord 键位绑定。serde default 兜底——旧配置无此字段时用各动作 default_key。
    /// 类型定义在 `domain::chord`（域层），此处仅引用以保持分层正确。
    #[serde(default)]
    pub bindings: crate::domain::chord::ChordBindings,
}

impl Default for ChordConfig {
    fn default() -> Self {
        Self {
            chord_enabled: false,
            chord_hint_visible: true,
            bindings: crate::domain::chord::ChordBindings::default(),
        }
    }
}

/// 三类 disable 黑名单聚合分片。**未来加新黑名单类型(如 `disabled_ai_providers`)
/// 直接进此片,不用穿透到其他片。**
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisableConfig {
    #[serde(default)]
    pub disabled_builtin_actions: Vec<String>,
    #[serde(default)]
    pub disabled_context_bindings: Vec<String>,
    #[serde(default)]
    pub disabled_chord_actions: Vec<String>,
}

fn default_language() -> String {
    "zh".to_string()
}

/// 日志级别默认值（旧配置无此字段时用 serde default 补，不丢其他配置）。
fn default_log_level() -> String {
    "error".to_string()
}

fn default_surface_takeover_enabled() -> bool {
    true
}

fn default_window_opacity() -> f64 {
    1.0
}

// ── AppConfig 新增字段默认值（0.5） ────────────────────────────────────────────────

fn default_theme() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_30() -> u32 {
    30
}

fn default_50() -> u32 {
    50
}

fn default_page_size() -> u32 {
    9
}

fn default_3() -> u32 {
    3
}

fn default_5() -> u32 {
    5
}

// ── Autosuggestion 默认值（0.8.1 §2.8）────────────────────────────────────────

fn default_autosuggest_min_score() -> f64 {
    0.7
}

fn default_autosuggest_tab_key() -> String {
    "Tab".to_string()
}

// ── 搜索引擎配置（三层独立控制）────────────────────────────────────────────────

/// 应用搜索配置（StartMenuEngine）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartMenuConfig {
    /// 是否启用应用搜索
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 开始菜单扫描深度
    #[serde(default = "default_3")]
    pub scan_depth: u32,
    /// 是否包含 UWP/MSIX 应用（通过 shell:AppsFolder 枚举）
    #[serde(default = "default_true")]
    pub include_uwp: bool,
}

impl Default for StartMenuConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_depth: 3,
            include_uwp: true,
        }
    }
}

/// 计算器配置（CalcEngine）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalcConfig {
    /// 是否启用计算器
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CalcConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_file_search_enabled() -> bool {
    true
}

fn default_data_source() -> String {
    "auto".to_string()
}

fn default_file_search_everything_port() -> u16 {
    80
}

fn default_file_search_max_results() -> u32 {
    20
}

fn default_local_dirs() -> Vec<String> {
    vec![
        "Desktop".to_string(),
        "Documents".to_string(),
        "Downloads".to_string(),
        "StartMenu".to_string(),
    ]
}

fn default_local_max_depth() -> u32 {
    3
}

fn default_local_cache_ttl() -> u64 {
    300
}

fn default_local_max_results() -> u32 {
    50
}

/// 文件搜索配置（FileEngine）。
///
/// 数据源模式 `data_source`：
/// - `"auto"`（默认）：优先 Everything，不可用时降级本地扫描
/// - `"everything"`：只用 Everything HTTP，不可用则无文件结果
/// - `"local"`：只用本地目录扫描，不尝试 Everything
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchConfig {
    /// 是否启用文件搜索（总开关）
    #[serde(default = "default_file_search_enabled")]
    pub enabled: bool,
    /// 数据源模式：auto / everything / local
    #[serde(default = "default_data_source")]
    pub data_source: String,
    /// Everything HTTP Server 端口
    #[serde(default = "default_file_search_everything_port")]
    pub everything_port: u16,
    /// 每次检索最大结果数（Everything）
    #[serde(default = "default_file_search_max_results")]
    pub max_results: u32,
    /// 本地扫描目录列表
    #[serde(default = "default_local_dirs")]
    pub local_dirs: Vec<String>,
    /// 本地扫描最大深度
    #[serde(default = "default_local_max_depth")]
    pub local_max_depth: u32,
    /// 本地缓存有效期（秒）
    #[serde(default = "default_local_cache_ttl")]
    pub local_cache_ttl_sec: u64,
    /// 本地搜索最多返回数
    #[serde(default = "default_local_max_results")]
    pub local_max_results: u32,
}

impl Default for FileSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            data_source: "auto".to_string(),
            everything_port: 80,
            max_results: 20,
            local_dirs: default_local_dirs(),
            local_max_depth: 3,
            local_cache_ttl_sec: 300,
            local_max_results: 50,
        }
    }
}

/// 应用配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 快捷键配置
    pub hotkey: HotkeyConfig,
    /// tap 阈值（毫秒）
    pub tap_threshold: u64,
    /// 看门狗 grace period（毫秒）
    pub grace_period: u64,
    /// 开机自启
    pub auto_start: bool,
    /// 语言（zh/en）
    pub language: String,
    /// 日志级别（error/info/debug/trace）
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 是否允许插件 takeover(接管返回区);false 时所有 takeover 降级 priority。
    #[serde(default = "default_surface_takeover_enabled")]
    pub surface_takeover_enabled: bool,
    /// 主题（auto/light/dark）
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 是否记录搜索历史
    #[serde(default = "default_true")]
    pub search_history_enabled: bool,
    /// 搜索历史保留天数
    #[serde(default = "default_30")]
    pub search_history_days: u32,
    /// 最多显示结果数
    #[serde(default = "default_50")]
    pub max_results: u32,
    /// 每页显示结果数
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// 是否启用主动建议（空 query 历史 top-N）
    #[serde(default = "default_false")]
    pub proactive_enabled: bool,
    /// 空 query 显示历史常用数
    #[serde(default = "default_5")]
    pub empty_query_topn: u32,
    /// 剪贴板历史配置
    #[serde(default)]
    pub clipboard: crate::infra::data::clipboard::ClipboardConfig,
    /// 用户禁用的内置动作 id 列表（0.8.0 §1.3）。
    /// 存 action id（如 `"shutdown"`），BuiltinEngine 召回时跳过。
    /// 默认空——所有动作默认启用；用户在设置页勾选后追加。
    #[serde(default)]
    pub disabled_builtin_actions: Vec<String>,
    /// 是否启用 Autosuggestion / Ghost Text（0.8.1 §2.8）。默认 true。
    /// 关闭后 `SearchService::search` 恒返回 `completion_hint: None`。
    #[serde(default = "default_true")]
    pub autosuggest_enabled: bool,
    /// Autosuggest fuzzy 阈值（0.8.1 §2.8），归一化到 [0,1]。默认 0.7。
    #[serde(default = "default_autosuggest_min_score")]
    pub autosuggest_min_score: f64,
    /// Autosuggest 接受补全的键位（0.8.1 §2.8）。默认 "Tab"。
    /// 前端消费；后端仅持久化 + 广播回前端。可选值当前仅 `"Tab"` / `"ArrowRight"`。
    #[serde(default = "default_autosuggest_tab_key")]
    pub autosuggest_tab_key: String,
    /// 用户禁用的 context binding key 列表（0.8.3 §4.6）。
    /// key 格式 `{target_id}::{trigger_key}`（如 `"builtin.translate::text_is_non_target_lang"`）。
    /// 存 binding key 字符串，`RuleRouter::match_context_hits` 命中即跳过。
    /// 默认空——所有 binding 默认启用；用户在「上下文感知」面板取消后追加。
    #[serde(default)]
    pub disabled_context_bindings: Vec<String>,
    /// Chord 模式总开关（0.8.5 §6.6）。**默认关闭**（0.8.7 起）：Chord 是"进阶交互"，
    /// 新用户 opt-in 更符合"不打扰"原则；老用户如已开启会尊重之。关闭时前端不响应 Alt+字母触发。
    #[serde(default = "default_false")]
    pub chord_enabled: bool,
    /// Chord 增强菜单可见性（0.8.5 §6.6）。默认 true——一旦启用 Chord，提示条是发现路径。
    #[serde(default = "default_true")]
    pub chord_hint_visible: bool,
    /// 0.10.7：chord 键位绑定（门面镜像，分片在 `ChordConfig.bindings`）。
    #[serde(default)]
    pub chord_bindings: crate::domain::chord::ChordBindings,
    /// 用户禁用的 Chord 动作 id 列表（0.8.5 §6.6）。存 action id，触发/列表时跳过。
    #[serde(default)]
    pub disabled_chord_actions: Vec<String>,
    /// 主窗口透明度 (0.0 ~ 1.0)，默认 1.0 不透明
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: HotkeyConfig::default(),
            tap_threshold: 300,
            grace_period: 500,
            auto_start: false,
            language: "zh".to_string(),
            log_level: "error".to_string(),
            surface_takeover_enabled: true,
            theme: default_theme(),
            search_history_enabled: default_true(),
            search_history_days: default_30(),
            max_results: default_50(),
            page_size: default_page_size(),
            proactive_enabled: default_false(),
            empty_query_topn: default_5(),
            clipboard: crate::infra::data::clipboard::ClipboardConfig::default(),
            disabled_builtin_actions: Vec::new(),
            autosuggest_enabled: true,
            autosuggest_min_score: 0.7,
            autosuggest_tab_key: "Tab".to_string(),
            disabled_context_bindings: Vec::new(),
            chord_enabled: false,
            chord_hint_visible: true,
            chord_bindings: crate::domain::chord::ChordBindings::default(),
            disabled_chord_actions: Vec::new(),
            window_opacity: default_window_opacity(),
        }
    }
}

/// 通用配置（0.5）：用户可调的外观与行为项。
/// 聚合更新（`update_general_config`）避免单字段命令爆炸。
/// `proactive_enabled` / `empty_query_topn` 属 P3 主动建议，暂不纳入（字段仍保留在 AppConfig）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// 主题：auto / light / dark
    pub theme: String,
    /// 是否记录搜索历史
    pub search_history_enabled: bool,
    /// 搜索历史保留天数（0 = 永久保留）
    pub search_history_days: u32,
    /// 融合后返回前端的最大结果数
    pub max_results: u32,
    /// 每页显示结果数
    pub page_size: u32,
}

impl From<&AppConfig> for GeneralConfig {
    fn from(c: &AppConfig) -> Self {
        Self {
            theme: c.theme.clone(),
            search_history_enabled: c.search_history_enabled,
            search_history_days: c.search_history_days,
            max_results: c.max_results,
            page_size: c.page_size,
        }
    }
}

// ── set_config 命令辅助结构体（0.8.6 P1-C 前端泛型化）─────────────────────────

/// Autosuggestion 更新参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutosuggestUpdate {
    pub enabled: bool,
    pub min_score: f64,
    pub tab_key: String,
}

/// Chord 开关更新参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChordTogglesUpdate {
    pub chord_enabled: bool,
    pub chord_hint_visible: bool,
}

/// 全局代理更新参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalProxyUpdate {
    pub http: String,
    pub https: String,
}

/// 插件配置更新参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfigUpdate {
    pub plugin_id: String,
    pub enabled: bool,
    pub settings: serde_json::Value,
}

// ── 配置操作函数 ────────────────────────────────────────────────────────────────

/// 初始化配置：首次运行写默认值 + 检测旧 `app_config` 单 key 触发迁移。
///
/// **迁移路径**（0.8.8 §8.7）：
/// - 检测 SQLite `config` 表是否存在旧 `app_config` 单 key
/// - 存在则读出解析成 `AppConfig`,拆到 6 片(`app.hotkey / app.appearance / app.search /
///   app.suggestion / app.chord / app.disable`) + `clipboard:config`
/// - 删除旧 `app_config` key
/// - 老用户升级透明,新用户直接走分片路径
pub async fn init_config(pool: &SqlitePool) -> Result<(), String> {
    // Step 1: 检测旧 KV 迁移
    if let Some(json) = crate::infra::data::history::get_config(pool, "app_config").await {
        tracing::info!("检测到旧 app_config 单 key,开始迁移到分片 KV");
        let legacy: AppConfig = serde_json::from_str(&json).unwrap_or_default();
        save_config(pool, &legacy).await?;
        crate::infra::data::history::delete_config(pool, "app_config")
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!("app_config 单 key 已拆分到 6 分片 + clipboard 独立 KV,旧 key 删除");
    }

    // Step 2: 首次运行:若分片全部空(等价于全新数据库或迁移前的老数据库),写默认值 + 系统语言
    let existing = crate::infra::data::history::get_all_config(pool).await;
    let has_any_shard = existing.contains_key("app.hotkey")
        || existing.contains_key("app.appearance")
        || existing.contains_key("app.search")
        || existing.contains_key("app.suggestion")
        || existing.contains_key("app.chord")
        || existing.contains_key("app.disable");
    if !has_any_shard {
        let mut config = AppConfig::default();
        config.language = crate::infra::platform::locale::detect_system_language();
        tracing::info!(language = %config.language, "首次运行,按系统语言设置默认语言");
        save_config(pool, &config).await?;
    }

    // Step 3: 一次性数据修正——旧版热键 key:"space" → " "(空格字符,匹配 vk_to_key)
    {
        let mut config = get_config(pool).await;
        if config.hotkey.key == "space" && config.hotkey.display.contains("Space") {
            config.hotkey.key = " ".to_string();
            save_config(pool, &config).await?;
            tracing::info!("迁移:修正热键 key 'space' → ' '");
        }
    }

    Ok(())
}

/// 获取完整配置（0.8.8 §8.7:门面 view,内部组合 6 分片 + clipboard 独立 KV）。
///
/// `AppConfig` struct 是**门面**,不是 KV 存储单位。读走
/// `ConfigStore::get::<HotkeyConfig / AppearanceConfig / ...>()` 分别拿 6 片,
/// 再加上 `ClipboardConfig`(第 7 独立 KV),组装成 `AppConfig`。**若所有分片
/// 都未存在,回落 `AppConfig::default()`——与旧行为一致**。
pub async fn get_config(pool: &SqlitePool) -> AppConfig {
    let hotkey = ConfigStore::get::<HotkeyConfig>(pool).await;
    let appearance = ConfigStore::get::<AppearanceConfig>(pool).await;
    let search = ConfigStore::get::<SearchConfig>(pool).await;
    let suggestion = ConfigStore::get::<SuggestionConfig>(pool).await;
    let chord = ConfigStore::get::<ChordConfig>(pool).await;
    let disable = ConfigStore::get::<DisableConfig>(pool).await;
    let clipboard = ConfigStore::get::<crate::infra::data::clipboard::ClipboardConfig>(pool).await;

    AppConfig {
        // ── HotkeyConfig 分片:hotkey 全字段 + tap/grace 展平到 AppConfig 门面 ──
        hotkey: HotkeyConfig {
            modifiers: hotkey.modifiers.clone(),
            key: hotkey.key.clone(),
            display: hotkey.display.clone(),
            tap_threshold: hotkey.tap_threshold,
            grace_period: hotkey.grace_period,
        },
        tap_threshold: hotkey.tap_threshold,
        grace_period: hotkey.grace_period,
        // ── AppearanceConfig 分片 ──────────────────────────────
        theme: appearance.theme,
        language: appearance.language,
        auto_start: appearance.auto_start,
        log_level: appearance.log_level,
        window_opacity: appearance.window_opacity,
        // ── SearchConfig 分片 ─────────────────────────────────
        surface_takeover_enabled: search.surface_takeover_enabled,
        search_history_enabled: search.search_history_enabled,
        search_history_days: search.search_history_days,
        max_results: search.max_results,
        page_size: search.page_size,
        // ── SuggestionConfig 分片 ──────────────────────────────
        autosuggest_enabled: suggestion.autosuggest_enabled,
        autosuggest_min_score: suggestion.autosuggest_min_score,
        autosuggest_tab_key: suggestion.autosuggest_tab_key,
        proactive_enabled: suggestion.proactive_enabled,
        empty_query_topn: suggestion.empty_query_topn,
        // ── ChordConfig 分片 ────────────────────────────────
        chord_enabled: chord.chord_enabled,
        chord_hint_visible: chord.chord_hint_visible,
        chord_bindings: chord.bindings.clone(),
        // ── DisableConfig 分片 ──────────────────────────────
        disabled_builtin_actions: disable.disabled_builtin_actions,
        disabled_context_bindings: disable.disabled_context_bindings,
        disabled_chord_actions: disable.disabled_chord_actions,
        // ── ClipboardConfig 独立 KV ─────────────────────────
        clipboard,
    }
}

/// 保存完整配置（0.8.8 §8.7:拆分回 6 分片 + clipboard 独立 KV,原子性由 SQLite 单表事务保障）。
///
/// 每次调用会写 7 次 SQL(6 分片 + clipboard),对不常见操作(设置页保存)可接受。
/// 0.9 前端接入通用 `set_config<K>` 后,`update_*` 内部可优化为"只写自己那片"。
pub async fn save_config(pool: &SqlitePool, config: &AppConfig) -> Result<(), String> {
    // Hotkey 分片:tap/grace 从 AppConfig 门面 top-level 或 hotkey 子字段任取(以门面 top-level 为准,
    // 因老 update_tap_threshold / update_grace_period 写的是 top-level 字段)
    let hotkey_shard = HotkeyConfig {
        modifiers: config.hotkey.modifiers.clone(),
        key: config.hotkey.key.clone(),
        display: config.hotkey.display.clone(),
        tap_threshold: config.tap_threshold,
        grace_period: config.grace_period,
    };
    ConfigStore::set(pool, &hotkey_shard).await?;

    ConfigStore::set(
        pool,
        &AppearanceConfig {
            theme: config.theme.clone(),
            language: config.language.clone(),
            auto_start: config.auto_start,
            log_level: config.log_level.clone(),
            window_opacity: config.window_opacity,
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &SearchConfig {
            search_history_enabled: config.search_history_enabled,
            search_history_days: config.search_history_days,
            max_results: config.max_results,
            page_size: config.page_size,
            surface_takeover_enabled: config.surface_takeover_enabled,
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &SuggestionConfig {
            autosuggest_enabled: config.autosuggest_enabled,
            autosuggest_min_score: config.autosuggest_min_score,
            autosuggest_tab_key: config.autosuggest_tab_key.clone(),
            proactive_enabled: config.proactive_enabled,
            empty_query_topn: config.empty_query_topn,
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &ChordConfig {
            chord_enabled: config.chord_enabled,
            chord_hint_visible: config.chord_hint_visible,
            bindings: config.chord_bindings.clone(),
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &DisableConfig {
            disabled_builtin_actions: config.disabled_builtin_actions.clone(),
            disabled_context_bindings: config.disabled_context_bindings.clone(),
            disabled_chord_actions: config.disabled_chord_actions.clone(),
        },
    )
    .await?;

    ConfigStore::set(pool, &config.clipboard).await?;

    Ok(())
}

/// 更新快捷键配置。
///
/// **0.8.8 §8.7**：只改 hotkey 三字段(modifiers/key/display),`HotkeyConfig` 里
/// tap_threshold / grace_period 字段忽略——它们由 `update_tap_threshold` / `update_grace_period`
/// 各自持有,避免"命令层构造 HotkeyConfig 时用 Default 覆盖用户已保存的 tap/grace"。
pub async fn update_hotkey(pool: &SqlitePool, hotkey: HotkeyConfig) -> Result<(), String> {
    let mut config = get_config(pool).await;
    // 只覆写按键三字段,保留 tap/grace(它们是分片的其他维度)
    config.hotkey.modifiers = hotkey.modifiers;
    config.hotkey.key = hotkey.key;
    config.hotkey.display = hotkey.display;
    save_config(pool, &config).await
}

/// 更新 tap 阈值。
pub async fn update_tap_threshold(pool: &SqlitePool, threshold: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.tap_threshold = threshold;
    save_config(pool, &config).await
}

/// 更新 grace period。
pub async fn update_grace_period(pool: &SqlitePool, period: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.grace_period = period;
    save_config(pool, &config).await
}

/// 更新开机自启设置。
pub async fn update_auto_start(pool: &SqlitePool, auto_start: bool) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.auto_start = auto_start;
    save_config(pool, &config).await
}

/// 更新语言设置。
pub async fn update_language(pool: &SqlitePool, language: String) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.language = language;
    save_config(pool, &config).await
}

/// 更新日志级别。
pub async fn update_log_level(pool: &SqlitePool, level: String) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.log_level = level;
    save_config(pool, &config).await
}

// ── 内置动作 disable 列表（0.8.0 §1.3）──────────────────────────────────────────

/// 获取当前 disable 的内置动作 id 列表（快照读）。
///
/// 设置页初始化时读一次（`list_builtin_actions` command），启动时读一次注入到
/// SearchService 内存快照。
pub async fn get_disabled_builtin_actions(pool: &SqlitePool) -> Vec<String> {
    get_config(pool).await.disabled_builtin_actions
}

/// 更新 disable 列表（设置页勾选后调用）。**幂等**：内部去重排序。
pub async fn update_disabled_builtin_actions(
    pool: &SqlitePool,
    disabled: Vec<String>,
) -> Result<(), String> {
    let mut normalized = disabled;
    normalized.sort();
    normalized.dedup();
    let mut config = get_config(pool).await;
    config.disabled_builtin_actions = normalized;
    save_config(pool, &config).await
}

// ── Autosuggestion 配置（0.8.1 §2.8）──────────────────────────────────────────

/// 更新 Autosuggestion 配置（设置页调用）。
///
/// 命令层调完后应同步调用 `SearchService::update_autosuggest_config` 让搜索热路径
/// 即时生效。tab_key 只影响前端键位监听，无热更新副作用。
pub async fn update_autosuggest_config(
    pool: &SqlitePool,
    enabled: bool,
    min_score: f64,
    tab_key: String,
) -> Result<(), String> {
    // 阈值夹到 [0, 1]，避免设置页误传导致命中永不触发
    let min_score = min_score.clamp(0.0, 1.0);
    let mut config = get_config(pool).await;
    config.autosuggest_enabled = enabled;
    config.autosuggest_min_score = min_score;
    config.autosuggest_tab_key = tab_key;
    save_config(pool, &config).await
}

// ── Context binding disable 列表（0.8.3 §4.6）──────────────────────────────────

/// 获取当前 disable 的 context binding key 列表（快照读）。
///
/// 设置页初始化时读一次，启动时读一次注入到 SearchService → RuleRouter 内存快照。
#[allow(dead_code)] // 设置页 API 预留（当前 commands 层直接读 AppConfig）
pub async fn get_disabled_context_bindings(pool: &SqlitePool) -> Vec<String> {
    get_config(pool).await.disabled_context_bindings
}

/// 更新 context binding disable 列表（设置页勾选后调用）。**幂等**：内部去重排序。
///
/// 命令层调完后应同步调用 `SearchService::update_disabled_context_bindings` 让搜索
/// 热路径立即生效。
pub async fn update_disabled_context_bindings(
    pool: &SqlitePool,
    disabled: Vec<String>,
) -> Result<(), String> {
    let mut normalized = disabled;
    normalized.sort();
    normalized.dedup();
    let mut config = get_config(pool).await;
    config.disabled_context_bindings = normalized;
    save_config(pool, &config).await
}

// ── Chord 配置（0.8.5 §6.6）──────────────────────────────────────────

/// 获取当前 disable 的 Chord 动作 id 列表（快照读）。
pub async fn get_disabled_chord_actions(pool: &SqlitePool) -> Vec<String> {
    get_config(pool).await.disabled_chord_actions
}

/// 获取 Chord 配置分片（0.10.7：chord 命令热路径用，只读 chord 这一片，不拉全量 AppConfig）。
pub async fn get_chord_config(pool: &SqlitePool) -> ChordConfig {
    ConfigStore::get::<ChordConfig>(pool).await
}

/// 更新 disable 列表（设置页勾选后调用）。**幂等**：内部去重排序。
pub async fn update_disabled_chord_actions(
    pool: &SqlitePool,
    disabled: Vec<String>,
) -> Result<(), String> {
    let mut normalized = disabled;
    normalized.sort();
    normalized.dedup();
    let mut config = get_config(pool).await;
    config.disabled_chord_actions = normalized;
    save_config(pool, &config).await
}

/// 获取 Chord 总开关 + 提示可见性（前端 shown 时读一次）。
#[allow(dead_code)] // 设置页 API 预留（当前 commands 层直接读 AppConfig）
pub async fn get_chord_toggles(pool: &SqlitePool) -> (bool, bool) {
    let cfg = get_config(pool).await;
    (cfg.chord_enabled, cfg.chord_hint_visible)
}

/// 更新 Chord 总开关 + 提示可见性（设置页保存时调）。
/// 触发 `blink://config-changed` 由命令层负责。
pub async fn update_chord_toggles(
    pool: &SqlitePool,
    chord_enabled: bool,
    chord_hint_visible: bool,
) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.chord_enabled = chord_enabled;
    config.chord_hint_visible = chord_hint_visible;
    save_config(pool, &config).await
}

/// 0.10.7：更新 chord 键位绑定。设置页改键后调用。
///
/// 只更新 bindings 字段，保留 chord_enabled / chord_hint_visible / disabled 列表。
pub async fn update_chord_bindings(
    pool: &SqlitePool,
    bindings: crate::domain::chord::ChordBindings,
) -> Result<(), String> {
    let mut chord = get_chord_config(pool).await;
    chord.bindings = bindings;
    ConfigStore::set(pool, &chord).await
}

/// 更新通用配置（主题 / 搜索历史 / 结果数）。仅持久化；
/// max_results 的运行时热更新由命令层通知 SearchService（热路径零 IO），
/// theme 由各窗口启动/shown 时读 config 生效（设置页本身即时预览）。
pub async fn update_general_config(
    pool: &SqlitePool,
    general: &GeneralConfig,
) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.theme = general.theme.clone();
    config.search_history_enabled = general.search_history_enabled;
    config.search_history_days = general.search_history_days;
    config.max_results = general.max_results;
    config.page_size = general.page_size;
    save_config(pool, &config).await
}

/// 获取引擎配置（通用 API）。
pub async fn get_engine_config(pool: &SqlitePool, engine_id: &str) -> Option<serde_json::Value> {
    let key = format!("engine:{}", engine_id);
    crate::infra::data::history::get_config(pool, &key)
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
}

/// 更新引擎配置（通用 API）。
pub async fn set_engine_config(
    pool: &SqlitePool,
    engine_id: &str,
    config: &serde_json::Value,
) -> Result<(), String> {
    let key = format!("engine:{}", engine_id);
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, &key, &json)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 获取文件搜索配置（key=`engine:file_search`）。不存在返回默认。
pub async fn get_file_search_config(pool: &SqlitePool) -> FileSearchConfig {
    get_engine_config(pool, "file_search")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

/// 更新文件搜索配置（写入 engine:file_search）。
pub async fn update_file_search(
    pool: &SqlitePool,
    file_search: FileSearchConfig,
) -> Result<(), String> {
    let engine_json = serde_json::to_value(file_search).map_err(|e| e.to_string())?;
    set_engine_config(pool, "file_search", &engine_json).await?;
    tracing::debug!("文件搜索配置已更新");
    Ok(())
}

// ── 应用搜索配置（0.8，engine:start_menu）──────────────────────────────────────

/// 获取应用搜索配置（key=`engine:start_menu`）。不存在返回默认。
pub async fn get_start_menu_config(pool: &SqlitePool) -> StartMenuConfig {
    get_engine_config(pool, "start_menu")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

/// 更新应用搜索配置。
pub async fn update_start_menu_config(
    pool: &SqlitePool,
    config: &StartMenuConfig,
) -> Result<(), String> {
    let json = serde_json::to_value(config).map_err(|e| e.to_string())?;
    set_engine_config(pool, "start_menu", &json).await?;
    tracing::debug!(
        enabled = config.enabled,
        scan_depth = config.scan_depth,
        "应用搜索配置已更新"
    );
    Ok(())
}

// ── 计算器配置（0.8，engine:calc）──────────────────────────────────────────────

/// 获取计算器配置（key=`engine:calc`）。不存在返回默认。
pub async fn get_calc_config(pool: &SqlitePool) -> CalcConfig {
    get_engine_config(pool, "calc")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

/// 更新计算器配置。
pub async fn update_calc_config(pool: &SqlitePool, config: &CalcConfig) -> Result<(), String> {
    let json = serde_json::to_value(config).map_err(|e| e.to_string())?;
    set_engine_config(pool, "calc", &json).await?;
    tracing::debug!(enabled = config.enabled, "计算器配置已更新");
    Ok(())
}

// ── 插件配置（0.5.1，见 0.5 设计 §2.4）───────────────────────────────────────────
//
// PluginConfig 只管 enabled + settings。trigger/surface 不在此——继续由 manifest
// 单一来源驱动 RuleRouter（详见 0.5 §2.4 评审修订说明）。

fn default_null() -> serde_json::Value {
    serde_json::Value::Null
}

/// 用户自定义触发关键字。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTrigger {
    /// 触发关键字
    pub keyword: String,
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 呈现模式覆盖：auto/inline/priority/takeover（None 用默认）
    #[serde(default)]
    pub surface: Option<String>,
}

/// 插件独立配置。settings 是 free-form JSON（manifest 声明 schema,core 只存不解释）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "CompatPluginConfig")]
pub struct PluginConfig {
    pub enabled: bool,
    #[serde(default = "default_null")]
    pub settings: serde_json::Value,
    /// 已禁用的默认触发词列表（用户 ban 掉的）
    #[serde(default)]
    #[allow(dead_code)] // serde 消费写入，业务逻辑通过 effective_triggers() 间接读
    pub disabled_default_triggers: Vec<String>,
    /// 用户自定义触发关键字
    #[serde(default)]
    pub custom_triggers: Vec<CustomTrigger>,
}

/// 兼容旧配置格式（用于数据迁移：disable_default_triggers: bool → disabled_default_triggers: Vec<String>）
#[derive(Debug, Deserialize)]
struct CompatPluginConfig {
    pub enabled: bool,
    #[serde(default = "default_null")]
    pub settings: serde_json::Value,
    // 旧格式：bool（是否禁用所有默认触发词）
    // serde 反序列化用，迁移逻辑已简化为直接读新格式——旧格式字段保留以兼容老配置 JSON
    #[serde(default)]
    #[allow(dead_code)]
    pub disable_default_triggers: Option<bool>,
    // 新格式：Vec<String>（被 ban 的具体触发词列表）
    #[serde(default)]
    pub disabled_default_triggers: Option<Vec<String>>,
    #[serde(default)]
    pub custom_triggers: Vec<CustomTrigger>,
}

impl From<CompatPluginConfig> for PluginConfig {
    fn from(compat: CompatPluginConfig) -> Self {
        // 优先用新格式；如果是旧格式且为 true，则暂时用空列表（下次保存时自动迁移）
        let disabled_default_triggers = compat.disabled_default_triggers.unwrap_or_default();

        Self {
            enabled: compat.enabled,
            settings: compat.settings,
            disabled_default_triggers,
            custom_triggers: compat.custom_triggers,
        }
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            settings: serde_json::Value::Null,
            disabled_default_triggers: Vec::new(),
            custom_triggers: Vec::new(),
        }
    }
}

/// 获取单个插件配置（key=`plugin:{id}`）。不存在返回 None。
pub async fn get_plugin_config(pool: &SqlitePool, plugin_id: &str) -> Option<PluginConfig> {
    let key = format!("plugin:{plugin_id}");
    crate::infra::data::history::get_config(pool, &key)
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
}

/// 设置插件配置（upsert,写 `plugin:{id}`）。
pub async fn set_plugin_config(
    pool: &SqlitePool,
    plugin_id: &str,
    config: &PluginConfig,
) -> Result<(), String> {
    let key = format!("plugin:{plugin_id}");
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, &key, &json)
        .await
        .map_err(|e| e.to_string())?;
    tracing::debug!(plugin_id, enabled = config.enabled, "插件配置已更新");
    Ok(())
}

/// 获取所有插件配置（key 前缀 `plugin:`）。返回 (plugin_id, config) 列表。
#[allow(dead_code)] // 插件系统骨架，0.3+ 启用
pub async fn get_all_plugin_config(pool: &SqlitePool) -> Vec<(String, PluginConfig)> {
    crate::infra::data::history::get_all_config(pool)
        .await
        .into_iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix("plugin:")?;
            let cfg = serde_json::from_str(&v).ok()?;
            Some((id.to_string(), cfg))
        })
        .collect()
}

impl PluginConfig {
    /// 合并 manifest triggers 和自定义 triggers，返回最终生效列表。
    pub fn effective_triggers(
        &self,
        manifest_triggers: &[crate::domain::plugin::PluginTrigger],
    ) -> Vec<crate::domain::plugin::PluginTrigger> {
        let mut result = Vec::new();

        // 1. 加默认 triggers（排除被 ban 的）
        for trigger in manifest_triggers {
            match trigger {
                crate::domain::plugin::PluginTrigger::Keyword { keyword, .. } => {
                    if self.disabled_default_triggers.contains(keyword) {
                        continue;
                    }
                    result.push(trigger.clone());
                }
                crate::domain::plugin::PluginTrigger::Regex { .. } => {
                    result.push(trigger.clone());
                }
                crate::domain::plugin::PluginTrigger::Context { .. } => {
                    // Context 触发不参与 disabled_default_triggers 过滤（用户想禁用整个 Context
                    // 触发应该走「插件 disable」，而不是关键词黑名单）。原样透传。
                    result.push(trigger.clone());
                }
            }
        }

        // 2. 加自定义 triggers
        for ct in &self.custom_triggers {
            if ct.enabled {
                result.push(crate::domain::plugin::PluginTrigger::Keyword {
                    keyword: ct.keyword.clone(),
                    exclusive: true,
                });
            }
        }

        result
    }
}

// ── Context 层配置（0.5.2，见 0.5 设计 §2.5）───────────────────────────────────

/// Context 层配置：控制唤起时的环境采集行为。
/// - enabled: 总开关，关闭后完全不采集
/// - clipboard_enabled: 是否采集剪贴板文本
/// - selection_enabled: 是否启用划词感知（鼠标划选文本 → UIA 抓取 → 缓存，供 invoke 时读取）
/// - sensitive_apps: 敏感应用进程名黑名单（如密码管理器），前台为这些应用时不采集（隐私保护）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub clipboard_enabled: bool,
    #[serde(default = "default_true")]
    pub selection_enabled: bool,
    #[serde(default)]
    pub sensitive_apps: Vec<String>,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            clipboard_enabled: true,
            selection_enabled: true,
            sensitive_apps: Vec::new(),
        }
    }
}

impl ContextConfig {
    /// 判断进程名是否在敏感应用黑名单（大小写不敏感）。
    pub fn is_sensitive(&self, process_name: &str) -> bool {
        let name = process_name.to_ascii_lowercase();
        self.sensitive_apps
            .iter()
            .any(|s| s.trim().eq_ignore_ascii_case(&name))
    }
}

/// 获取 Context 配置（key=`context:config`）。不存在或解析失败返回默认。
pub async fn get_context_config(pool: &SqlitePool) -> ContextConfig {
    crate::infra::data::history::get_config(pool, "context:config")
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// 设置 Context 配置（upsert,写 `context:config`）。
pub async fn set_context_config(pool: &SqlitePool, config: &ContextConfig) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, "context:config", &json)
        .await
        .map_err(|e| e.to_string())?;
    tracing::debug!(
        enabled = config.enabled,
        clipboard = config.clipboard_enabled,
        selection = config.selection_enabled,
        sensitive_count = config.sensitive_apps.len(),
        "Context 配置已更新"
    );
    Ok(())
}

// ── TODO: 方案 B - Trait 抽象（后续重构）────────────────────────────────────────
//
// 当需要支持多平台时，可以将配置管理抽象为 trait：
//
// ```rust
// pub trait ConfigManager {
//     async fn get_config(&self) -> AppConfig;
//     async fn save_config(&self, config: &AppConfig) -> Result<(), String>;
//     async fn get_hotkey(&self) -> HotkeyConfig;
//     async fn update_hotkey(&self, hotkey: HotkeyConfig) -> Result<(), String>;
//     // ... 其他配置操作
// }
//
// // 每个平台实现自己的 ConfigManager
// pub struct SqliteConfigManager { pool: SqlitePool }
// pub struct FileConfigManager { path: PathBuf }  // macOS/Linux 可能用文件
// ```
//
// 这样可以：
// 1. 支持不同的存储后端（SQLite、文件、注册表等）
// 2. 支持平台特定的配置项
// 3. 更容易进行单元测试（mock ConfigManager）
//

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_key_app_shards() {
        // 0.8.8 §8.7:AppConfig 拆为 6 片,验证各分片 KV key
        assert_eq!(HotkeyConfig::KEY, "app.hotkey");
        assert_eq!(AppearanceConfig::KEY, "app.appearance");
        assert_eq!(SearchConfig::KEY, "app.search");
        assert_eq!(SuggestionConfig::KEY, "app.suggestion");
        assert_eq!(ChordConfig::KEY, "app.chord");
        assert_eq!(DisableConfig::KEY, "app.disable");
    }

    #[test]
    fn config_key_start_menu() {
        assert_eq!(StartMenuConfig::KEY, "engine:start_menu");
    }

    #[test]
    fn config_key_calc() {
        assert_eq!(CalcConfig::KEY, "engine:calc");
    }

    #[test]
    fn config_key_context() {
        assert_eq!(ContextConfig::KEY, "context:config");
    }

    #[test]
    fn config_key_clipboard() {
        // 0.8.8 §8.7:clipboard 从 nested 提升为独立 KV(不属于 app.* 命名空间)
        assert_eq!(
            <crate::infra::data::clipboard::ClipboardConfig as ConfigKey>::KEY,
            "clipboard:config"
        );
    }

    #[test]
    fn app_config_default_serde_roundtrip() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.language, config.language);
        assert_eq!(parsed.theme, config.theme);
        assert_eq!(parsed.hotkey.key, config.hotkey.key);
    }

    #[test]
    fn app_config_from_default_json() {
        // 验证默认值 JSON 能正确反序列化
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.language, "zh");
        assert_eq!(parsed.theme, "auto");
        assert!(parsed.autosuggest_enabled);
        assert!((parsed.autosuggest_min_score - 0.7).abs() < 1e-9);
        assert_eq!(parsed.autosuggest_tab_key, "Tab");
    }

    #[test]
    fn shard_defaults_match_appconfig_default() {
        // 6 分片 + clipboard 的 Default 值必须与 AppConfig::default() 对应字段一致
        // ——否则首次从空数据库组装出的 AppConfig 会与旧行为不一致。
        let app = AppConfig::default();
        let hotkey = HotkeyConfig::default();
        let app_shard = AppearanceConfig::default();
        let search = SearchConfig::default();
        let suggestion = SuggestionConfig::default();
        let chord = ChordConfig::default();
        let disable = DisableConfig::default();

        assert_eq!(hotkey.key, app.hotkey.key);
        assert_eq!(hotkey.tap_threshold, app.tap_threshold);
        assert_eq!(hotkey.grace_period, app.grace_period);
        assert_eq!(app_shard.theme, app.theme);
        assert_eq!(app_shard.language, app.language);
        assert_eq!(app_shard.log_level, app.log_level);
        assert_eq!(app_shard.auto_start, app.auto_start);
        assert_eq!(search.max_results, app.max_results);
        assert_eq!(
            search.surface_takeover_enabled,
            app.surface_takeover_enabled
        );
        assert_eq!(suggestion.autosuggest_enabled, app.autosuggest_enabled);
        assert_eq!(suggestion.proactive_enabled, app.proactive_enabled);
        assert_eq!(chord.chord_enabled, app.chord_enabled);
        assert_eq!(chord.chord_hint_visible, app.chord_hint_visible);
        assert_eq!(
            disable.disabled_builtin_actions,
            app.disabled_builtin_actions
        );
    }

    // ── 分片迁移集成测试（0.8.8 §8.7）──────────────────────────────────
    //
    // 用 in-memory SQLite 池验证:老 app_config 单 key → 6 分片 + clipboard 独立 KV 迁移路径。
    // 用 `tauri::async_runtime::block_on` 桥接,与项目其他 async 单测保持一致(见 intent/mod.rs)。

    async fn in_memory_pool() -> SqlitePool {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("create config table");
        pool
    }

    #[test]
    fn get_config_on_empty_db_returns_defaults() {
        tauri::async_runtime::block_on(async {
            let pool = in_memory_pool().await;
            let cfg = get_config(&pool).await;
            assert_eq!(cfg.theme, AppConfig::default().theme);
            assert_eq!(cfg.hotkey.key, AppConfig::default().hotkey.key);
            assert_eq!(cfg.tap_threshold, 300);
            assert_eq!(cfg.grace_period, 500);
        });
    }

    #[test]
    fn save_and_get_config_roundtrip() {
        tauri::async_runtime::block_on(async {
            let pool = in_memory_pool().await;
            let mut cfg = AppConfig::default();
            cfg.theme = "light".to_string();
            cfg.language = "en".to_string();
            cfg.max_results = 42;
            cfg.autosuggest_min_score = 0.85;
            cfg.chord_enabled = true;
            cfg.disabled_builtin_actions = vec!["shutdown".to_string()];
            cfg.tap_threshold = 250;
            cfg.hotkey.key = "F1".to_string();
            save_config(&pool, &cfg).await.unwrap();

            let loaded = get_config(&pool).await;
            assert_eq!(loaded.theme, "light");
            assert_eq!(loaded.language, "en");
            assert_eq!(loaded.max_results, 42);
            assert!((loaded.autosuggest_min_score - 0.85).abs() < 1e-9);
            assert!(loaded.chord_enabled);
            assert_eq!(loaded.disabled_builtin_actions, vec!["shutdown"]);
            assert_eq!(loaded.tap_threshold, 250);
            assert_eq!(loaded.hotkey.key, "F1");
            // Hotkey 分片里的 tap_threshold 与门面 top-level 应一致
            assert_eq!(loaded.hotkey.tap_threshold, 250);
        });
    }

    #[test]
    fn shards_persist_to_distinct_kv_keys() {
        tauri::async_runtime::block_on(async {
            // save_config 后,SQLite 里应有 7 个独立 key(6 分片 + clipboard),而不是单个 app_config
            let pool = in_memory_pool().await;
            save_config(&pool, &AppConfig::default()).await.unwrap();

            let all = crate::infra::data::history::get_all_config(&pool).await;
            assert!(all.contains_key("app.hotkey"), "app.hotkey 分片应存在");
            assert!(
                all.contains_key("app.appearance"),
                "app.appearance 分片应存在"
            );
            assert!(all.contains_key("app.search"), "app.search 分片应存在");
            assert!(
                all.contains_key("app.suggestion"),
                "app.suggestion 分片应存在"
            );
            assert!(all.contains_key("app.chord"), "app.chord 分片应存在");
            assert!(all.contains_key("app.disable"), "app.disable 分片应存在");
            assert!(
                all.contains_key("clipboard:config"),
                "clipboard 独立 KV 应存在"
            );
            assert!(!all.contains_key("app_config"), "旧单 key 不应重现");
        });
    }

    #[test]
    fn legacy_app_config_migrates_to_shards() {
        tauri::async_runtime::block_on(async {
            // 模拟老用户升级:预置 app_config 单 key,init_config 后应拆到分片 + 删旧 key
            let pool = in_memory_pool().await;

            // 手写老格式 json(带 6 分片对应的关键字段)
            let legacy_json = r#"{
                "hotkey": {"modifiers": ["ctrl", "alt"], "key": "k", "display": "Ctrl+Alt+K"},
                "tap_threshold": 250,
                "grace_period": 400,
                "auto_start": true,
                "language": "en",
                "log_level": "debug",
                "surface_takeover_enabled": false,
                "theme": "gruvbox",
                "search_history_enabled": false,
                "search_history_days": 60,
                "max_results": 25,
                "page_size": 7,
                "proactive_enabled": true,
                "empty_query_topn": 3,
                "clipboard": {"enabled": false, "max_items": 100, "retention_days": 3, "search_enabled": false, "blacklist_keywords": []},
                "disabled_builtin_actions": ["shutdown", "restart"],
                "autosuggest_enabled": false,
                "autosuggest_min_score": 0.9,
                "autosuggest_tab_key": "ArrowRight",
                "disabled_context_bindings": ["builtin.translate::text_is_non_target_lang"],
                "chord_enabled": true,
                "chord_hint_visible": false,
                "disabled_chord_actions": ["screenshot"]
            }"#;
            crate::infra::data::history::set_config(&pool, "app_config", legacy_json)
                .await
                .unwrap();

            init_config(&pool).await.unwrap();

            // Step A: 旧 key 应被删除
            assert!(
                crate::infra::data::history::get_config(&pool, "app_config")
                    .await
                    .is_none(),
                "app_config 单 key 应在迁移后删除"
            );

            // Step B: 分片应包含迁移后的数据
            let all = crate::infra::data::history::get_all_config(&pool).await;
            assert!(all.contains_key("app.hotkey"));
            assert!(all.contains_key("clipboard:config"));

            // Step C: 通过 get_config 组装的门面应还原所有字段
            let cfg = get_config(&pool).await;
            assert_eq!(cfg.hotkey.modifiers, vec!["ctrl", "alt"]);
            assert_eq!(cfg.hotkey.key, "k");
            assert_eq!(cfg.tap_threshold, 250);
            assert_eq!(cfg.grace_period, 400);
            assert!(cfg.auto_start);
            assert_eq!(cfg.language, "en");
            assert_eq!(cfg.log_level, "debug");
            assert!(!cfg.surface_takeover_enabled);
            assert_eq!(cfg.theme, "gruvbox");
            assert!(!cfg.search_history_enabled);
            assert_eq!(cfg.max_results, 25);
            assert!(cfg.proactive_enabled);
            assert_eq!(cfg.empty_query_topn, 3);
            assert!(!cfg.clipboard.enabled);
            assert_eq!(cfg.clipboard.max_items, 100);
            assert_eq!(cfg.disabled_builtin_actions, vec!["shutdown", "restart"]);
            assert!(!cfg.autosuggest_enabled);
            assert!((cfg.autosuggest_min_score - 0.9).abs() < 1e-9);
            assert_eq!(cfg.autosuggest_tab_key, "ArrowRight");
            assert_eq!(
                cfg.disabled_context_bindings,
                vec!["builtin.translate::text_is_non_target_lang"]
            );
            assert!(cfg.chord_enabled);
            assert!(!cfg.chord_hint_visible);
            assert_eq!(cfg.disabled_chord_actions, vec!["screenshot"]);
        });
    }

    #[test]
    fn update_hotkey_preserves_tap_grace() {
        tauri::async_runtime::block_on(async {
            // update_hotkey 只该动 modifiers/key/display,不该覆盖 tap/grace(回归 bug 防护)
            let pool = in_memory_pool().await;
            let mut cfg = AppConfig::default();
            cfg.tap_threshold = 250;
            cfg.grace_period = 700;
            save_config(&pool, &cfg).await.unwrap();

            // 命令层构造 HotkeyConfig 时用 Default(300/500),但 update_hotkey 内部应保留原 tap/grace
            let new_hotkey = HotkeyConfig {
                modifiers: vec!["ctrl".to_string()],
                key: "F2".to_string(),
                display: "Ctrl+F2".to_string(),
                ..Default::default() // tap=300 / grace=500,但不该覆盖
            };
            update_hotkey(&pool, new_hotkey).await.unwrap();

            let loaded = get_config(&pool).await;
            assert_eq!(loaded.hotkey.key, "F2");
            assert_eq!(
                loaded.tap_threshold, 250,
                "update_hotkey 不该覆盖 tap_threshold"
            );
            assert_eq!(
                loaded.grace_period, 700,
                "update_hotkey 不该覆盖 grace_period"
            );
        });
    }
}
