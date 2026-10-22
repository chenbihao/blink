//! 更新提醒（0.26.20）：后台静默检查 + 提示状态接线。
//!
//! 职责：
//! - 检查结果的落库与内存状态（`last_check` 节流 / `latest_version` 已检测版本 /
//!   `dismissed_version` 已读屏蔽），持久化在 `app.update` 分片
//! - 后台任务：启动 ≥2min 首查、此后 24h 节流、失败仅 warn 零 UI 打扰
//! - 「可用更新」判定（供 [`crate::app::window_orchestrator`] 盖章进
//!   `AwarenessSnapshot.available_update` → 建议槽 UpdateProducer）与托盘版本提示
//! - `dismiss` 与「自动检查更新」总开关写入
//!
//! 网络检查本体是 `commands::diagnostic::perform_update_check`（与设置页手动检查
//! 共用 reqwest + 全局代理链路）；本模块不重复请求逻辑，只消费其结果。

use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use crate::domain::config::shards::UpdateConfig;
use crate::domain::config::store::ConfigStore;

/// 当前应用版本（与 check_update 命令的 `env!("CARGO_PKG_VERSION")` 同源）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 后台检查节奏参数（0.26.20）。生产默认：首查延迟 2min、周期 1h、节流 24h；
/// 单测可注入短值验证时间逻辑。
pub(crate) struct BackgroundParams {
    pub initial_delay: Duration,
    pub period: Duration,
    pub throttle: Duration,
}

impl Default for BackgroundParams {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(2 * 60),
            period: Duration::from_secs(60 * 60),
            throttle: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// 内存状态快照——`app.manage(Arc<RwLock<UpdateReminderState>>)` 托管，
/// 搜索唤起路径读它给快照盖章（零 IO）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateReminderState {
    pub auto_check_enabled: bool,
    /// Unix 秒；0 = 从未检查。
    pub last_check: i64,
    pub latest_version: String,
    pub dismissed_version: String,
}

impl From<UpdateConfig> for UpdateReminderState {
    fn from(cfg: UpdateConfig) -> Self {
        Self {
            auto_check_enabled: cfg.auto_check_enabled,
            last_check: cfg.last_check,
            latest_version: cfg.latest_version,
            dismissed_version: cfg.dismissed_version,
        }
    }
}

// ── 纯判定逻辑（单测锚点）──────────────────────────────────────────────────

/// 后台是否应发起检查：总开关开 && 距上次检查 ≥ 节流窗（含「从未检查」）。
/// 失败也推进 `last_check`——24h 内最多一次尝试，断网场景不每小时重试打
/// GitHub 匿名限流（60 次/小时）。
pub(crate) fn should_check(state: &UpdateReminderState, now_secs: i64, throttle: Duration) -> bool {
    if !state.auto_check_enabled {
        return false;
    }
    let throttle_secs = throttle.as_secs() as i64;
    now_secs.saturating_sub(state.last_check) >= throttle_secs
}

/// 建议槽可见的「可用更新」版本：自动检查开 && 检测到新版 && 未被 dismiss。
/// 返回引用避免热路径克隆；调用方按需 to_string。
pub fn available_version(state: &UpdateReminderState) -> Option<&str> {
    if !state.auto_check_enabled {
        return None;
    }
    let latest = state.latest_version.as_str();
    if latest.is_empty()
        || !version_gt(latest, CURRENT_VERSION)
        || latest == state.dismissed_version
    {
        return None;
    }
    Some(latest)
}

/// 托盘「检查更新」文案携带的版本提示：不受 dismiss 影响（主动点开菜单才可见，
/// 始终显示最新已知版本），只受总开关与「是否检测到新版」约束。
pub fn tray_version_hint(state: &UpdateReminderState) -> Option<&str> {
    if !state.auto_check_enabled {
        return None;
    }
    let latest = state.latest_version.as_str();
    if latest.is_empty() || !version_gt(latest, CURRENT_VERSION) {
        return None;
    }
    Some(latest)
}

/// 应用一次检查结果（纯转移）：`now` 推进 `last_check`；成功（`latest = Some`）
/// 时更新 `latest_version`，失败保持旧值。
pub(crate) fn apply_check_outcome(
    state: &UpdateReminderState,
    latest: Option<&str>,
    now_secs: i64,
) -> UpdateReminderState {
    let mut next = state.clone();
    next.last_check = now_secs;
    if let Some(v) = latest {
        next.latest_version = v.to_string();
    }
    next
}

/// 记录 dismiss（纯转移）：仅当已知版本确实构成可用更新时把 `dismissed_version`
/// 置为该版本；返回是否生效（无可用版本时 no-op）。
pub(crate) fn apply_dismiss(state: &UpdateReminderState) -> (UpdateReminderState, bool) {
    if available_version(state).is_some() {
        let mut next = state.clone();
        next.dismissed_version = state.latest_version.clone();
        (next, true)
    } else {
        (state.clone(), false)
    }
}

/// semver 比较；非标准版本号回退三段数字比较（从 diagnostic.rs 迁入，
/// check_update 命令与提醒状态共用同一判定）。
pub(crate) fn version_gt(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(va), Ok(vb)) => va > vb,
        _ => version_gt_fallback(a, b),
    }
}

/// Fallback：非标准版本号的简单数字比较，取前三段，缺失按 0 算。
fn version_gt_fallback(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let parts: Vec<u64> = s.split('.').filter_map(|p| p.parse().ok()).collect();
        (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
        )
    };
    parse(a) > parse(b)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── 托管状态读写 ────────────────────────────────────────────────────────────

/// 从 `app.update` 分片加载内存状态（启动一次）。
pub async fn load_state(pool: &sqlx::SqlitePool) -> UpdateReminderState {
    UpdateReminderState::from(ConfigStore::get::<UpdateConfig>(pool).await)
}

fn read_state(app: &tauri::AppHandle) -> UpdateReminderState {
    app.try_state::<Arc<std::sync::RwLock<UpdateReminderState>>>()
        .map(|c| c.read().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default()
}

fn write_state(app: &tauri::AppHandle, state: UpdateReminderState) {
    if let Some(cell) = app.try_state::<Arc<std::sync::RwLock<UpdateReminderState>>>() {
        *cell.write().unwrap_or_else(|e| e.into_inner()) = state;
    }
}

/// 唤起路径的快照盖章值：当前可见的可用更新版本（managed cell 零 IO 读取）。
pub fn snapshot_update_hint(app: &tauri::AppHandle) -> Option<String> {
    available_version(&read_state(app)).map(|v| v.to_string())
}

async fn persist_state(app: &tauri::AppHandle, state: &UpdateReminderState) {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let cfg = UpdateConfig {
        auto_check_enabled: state.auto_check_enabled,
        last_check: state.last_check,
        latest_version: state.latest_version.clone(),
        dismissed_version: state.dismissed_version.clone(),
    };
    if let Err(e) = ConfigStore::set(pool, &cfg).await {
        tracing::warn!(error = %e, "update_reminder: 状态持久化失败（内存态仍生效）");
    }
}

/// 当前语言下重建托盘菜单（携带版本提示）。托盘菜单是 Rust 侧静态构建，
/// 版本提示出现/消失时由状态转移入口触发；语言从 `app.appearance` 分片读取
///（语言切换自身的重建由 set_config "language" 分支负责）。
async fn rebuild_tray(app: &tauri::AppHandle) {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let lang = ConfigStore::get::<crate::domain::config::shards::AppearanceConfig>(pool)
        .await
        .language;
    let state = read_state(app);
    crate::app::tray::rebuild_menu(app, &lang, tray_version_hint(&state));
}

// ── 状态转移入口（command / 后台任务共用）────────────────────────────────────

/// 记录一次检查结果：推进 `last_check`，成功时更新 `latest_version`；
/// 托盘版本提示变化时重建菜单。
pub async fn record_check_outcome(app: &tauri::AppHandle, latest: Option<&str>) {
    let old = read_state(app);
    let next = apply_check_outcome(&old, latest, unix_now());
    let hint_changed = tray_version_hint(&old) != tray_version_hint(&next);
    persist_state(app, &next).await;
    write_state(app, next);
    if hint_changed {
        rebuild_tray(app).await;
    }
}

/// 用户显式关闭版本提示（建议槽 × 按钮 / Tab 采纳后前端补记）：
/// 把当前可用版本记入 `dismissed_version`，本版本不再出现、发布更新版本后自动恢复。
/// 同时清除 SearchService 快照中的标记——本窗口会话内迟到的搜索响应不得复活该建议。
/// 返回是否生效。
pub async fn dismiss_latest(app: &tauri::AppHandle) -> bool {
    let old = read_state(app);
    let (next, applied) = apply_dismiss(&old);
    if !applied {
        return false;
    }
    persist_state(app, &next).await;
    write_state(app, next);
    if let Some(ss) = app.try_state::<Arc<crate::domain::search::SearchService>>() {
        ss.clear_available_update();
    }
    true
}

/// 「自动检查更新」总开关写入（设置页关于 tab）。关闭后后台任务停跑、
/// 建议槽与托盘版本提示不再出现；手动检查（设置页按钮 / 主窗口动作）不拦。
/// 关闭时同步清除 SearchService 快照标记（当前窗口会话内不再显示）。
pub async fn set_auto_check_enabled(app: &tauri::AppHandle, enabled: bool) {
    let old = read_state(app);
    let mut next = old.clone();
    next.auto_check_enabled = enabled;
    let hint_changed = tray_version_hint(&old) != tray_version_hint(&next);
    persist_state(app, &next).await;
    write_state(app, next);
    if !enabled {
        if let Some(ss) = app.try_state::<Arc<crate::domain::search::SearchService>>() {
            ss.clear_available_update();
        }
    }
    if hint_changed {
        rebuild_tray(app).await;
    }
    tracing::info!(enabled, "自动检查更新开关已切换");
}

/// 打开设置窗口并定位到「关于」Tab（托盘「检查更新」/ 建议采纳 / 内置动作共用；
/// 关于 tab 打开即静默自动检查并渲染）。
pub fn open_settings_about(app: &tauri::AppHandle) {
    crate::infra::platform::window::open_settings(app);
    if let Some(w) = app.get_webview_window("settings") {
        // HTML 异步加载，需延迟等 DOM 就绪后再切 Tab（与 open_settings_tab 的 eval 模式一致）
        let _ = w.eval(
            "setTimeout(() => document.querySelector('.tab[data-tab=\"about\"]')?.click(), 300)",
        );
    }
}

// ── 主窗口「检查更新」动作的结果条目（0.26.20 修订）────────────────────────

/// 检查结果 → 标准结果条目数组（AppEntry 兼容形状），经 `run_builtin_action`
/// 返回值回流主窗口结果区，前端按常规条目渲染并可再次触发。
///
/// 分层约定（与插件/引擎条目一致）：**条目数据由后端出**（含本地化文案，
/// 按 `AppearanceConfig.language` 选 zh/en，与托盘菜单同惯例），前端纯渲染。
/// `icon` 字段 = Lucide 图标名——无 lnk_path/icon_path 可懒加载的合成条目据此
/// 挂图标（前端 createItem 通用分支，取代逐 source 短路）。
pub(crate) fn check_result_items(
    outcome: &crate::app::commands::UpdateCheckOutcome,
    lang: &str,
) -> Vec<serde_json::Value> {
    let en = lang.eq_ignore_ascii_case("en");
    let item = |name: String, desc: &str, icon: &str, is_error: bool, actions: Vec<serde_json::Value>| {
        serde_json::json!({
            "name": name,
            "pinyin_name": "",
            "pinyin_full": "",
            "lnk_path": "",
            "is_calc": false,
            "score": 999,
            "is_placeholder": false,
            "is_error": is_error,
            "source": "update",
            "icon": icon,
            "description": desc,
            "actions": actions,
        })
    };

    if let Some(err) = &outcome.error {
        let name = if en {
            "Update check failed".to_string()
        } else {
            "检查更新失败".to_string()
        };
        return vec![item(name, err, "triangle-alert", true, Vec::new())];
    }
    if outcome.has_update {
        let name = if en {
            format!("New version {} available", outcome.latest_version)
        } else {
            format!("发现新版本 {}", outcome.latest_version)
        };
        // 0.27.4：镜像降级（主站不可达、镜像判出版本）时说明不可用，如实标注
        let desc = match (en, outcome.degraded) {
            (true, true) => "Press Enter to view update details (release notes unavailable; checked via mirror)",
            (true, false) => "Press Enter to view update details",
            (false, true) => "回车查看更新内容（更新说明暂不可用，经镜像检查）",
            (false, false) => "回车查看更新内容",
        };
        // settings kind = 纯 UI 导航（Interaction 层协议，不伪装 Capability），
        // payload = 目标 tab（后端命令带白名单校验）
        return vec![item(
            name,
            desc,
            "download",
            false,
            vec![serde_json::json!({"kind": "settings", "payload": "about"})],
        )];
    }
    let name = if en {
        format!("You're on the latest version ({})", outcome.current_version)
    } else {
        format!("已是最新版本（{}）", outcome.current_version)
    };
    let desc = match (en, outcome.degraded) {
        (true, true) => "Checked via mirror; release notes unavailable",
        (true, false) => "Latest release on GitHub",
        (false, true) => "经镜像检查（主站不可达），更新说明暂不可用",
        (false, false) => "GitHub 最新发布版本",
    };
    vec![item(
        name,
        desc,
        "check",
        false,
        vec![serde_json::json!({"kind": "copy", "payload": outcome.current_version})],
    )]
}

// ── 后台静默检查任务 ────────────────────────────────────────────────────────

/// 启动后台静默检查：延迟 ≥2min 首查（绝不进启动热路径），此后每 1h 醒来一次、
/// 按 24h 节流判定是否真查；失败仅 warn 日志，零 UI 打扰。
/// 请求走 `perform_update_check`（reqwest + 全局代理），与设置页手动检查同链路。
pub fn spawn_background_task(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let params = BackgroundParams::default();
        tokio::time::sleep(params.initial_delay).await;
        loop {
            let state = read_state(&app);
            if should_check(&state, unix_now(), params.throttle) {
                tracing::debug!("update_reminder: 后台静默检查开始");
                let outcome =
                    crate::app::commands::perform_update_check(&app).await;
                if let Some(err) = &outcome.error {
                    tracing::warn!(error = %err, "update_reminder: 后台检查更新失败（不打扰用户）");
                } else if outcome.has_update {
                    tracing::info!(
                        current = CURRENT_VERSION,
                        latest = %outcome.latest_version,
                        "update_reminder: 检测到新版本"
                    );
                } else {
                    tracing::debug!(current = CURRENT_VERSION, "update_reminder: 已是最新版本");
                }
                // 失败也推进 last_check（24h 内最多一次尝试，防限流）
                let latest = outcome.error.is_none().then_some(outcome.latest_version.as_str());
                record_check_outcome(&app, latest).await;
            }
            tokio::time::sleep(params.period).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(latest: &str, dismissed: &str, auto: bool, last_check: i64) -> UpdateReminderState {
        UpdateReminderState {
            auto_check_enabled: auto,
            last_check,
            latest_version: latest.to_string(),
            dismissed_version: dismissed.to_string(),
        }
    }

    /// 节流逻辑：从未检查 → 到期即查；24h 内不重复；总开关关闭恒不查；
    /// 时钟回拨（now < last_check）不触发。
    #[test]
    fn should_check_throttle() {
        const DAY: u64 = 24 * 60 * 60;
        let day = DAY as i64;
        let auto_on = state("0.0.1", "", true, 0);
        assert!(should_check(&auto_on, day, Duration::from_secs(DAY)), "从未检查（last=0）视为到期");
        assert!(!should_check(&auto_on, day - 1, Duration::from_secs(DAY)), "24h 内不重复");
        let recent = state("0.0.1", "", true, day);
        assert!(!should_check(&recent, day + day - 1, Duration::from_secs(DAY)));
        assert!(should_check(&recent, day + day, Duration::from_secs(DAY)), "满 24h 到期");
        let auto_off = state("0.0.1", "", false, 0);
        assert!(!should_check(&auto_off, day * 365, Duration::from_secs(DAY)), "总开关关闭恒不查");
        assert!(!should_check(&recent, 0, Duration::from_secs(DAY)), "时钟回拨不触发");
    }

    /// 建议槽三态：检测到新版且未 dismiss → Some；dismiss 后 None；更新版本恢复。
    /// （三态转移：未检测 → 检测到 → 已读屏蔽 → 发布更新版本）
    #[test]
    fn available_version_three_state_transition() {
        // CURRENT_VERSION 是本 crate 版本（测试构建取 Cargo.toml），用相对比较构造
        let newer = format!("{}.{}.{}", 999, 0, 0);
        let newer2 = format!("{}.{}.{}", 999, 0, 1);

        // ① 未检测（旧版本/空）→ None
        let s = state("", "", true, 0);
        assert!(available_version(&s).is_none());
        let s = state("0.0.1", "", true, 0);
        assert!(available_version(&s).is_none(), "latest <= current 不提示");

        // ② 检测到新版 → Some
        let s = state(&newer, "", true, 0);
        assert_eq!(available_version(&s), Some(newer.as_str()));

        // ③ dismiss → None；托盘仍显示（不受 dismiss 影响）
        let (s2, applied) = apply_dismiss(&s);
        assert!(applied);
        assert!(available_version(&s2).is_none());
        assert_eq!(tray_version_hint(&s2), Some(newer.as_str()));

        // ④ 发布更新版本（后台再次检查写入 newer2）→ 自动恢复
        let s3 = apply_check_outcome(&s2, Some(&newer2), 12345);
        assert_eq!(available_version(&s3), Some(newer2.as_str()));

        // 总开关关闭 → 两路提示都消失（dismiss 不 rescue）
        let s4 = UpdateReminderState {
            auto_check_enabled: false,
            ..s3.clone()
        };
        assert!(available_version(&s4).is_none());
        assert!(tray_version_hint(&s4).is_none());
        // 保持变量已读，避免未使用警告
        let _ = &s;
    }

    /// 检查结果转移：成功更新 latest；失败保持旧值；两者都推进 last_check。
    #[test]
    fn apply_check_outcome_transitions() {
        let base = state("1.0.0", "", true, 100);
        let ok = apply_check_outcome(&base, Some("2.0.0"), 200);
        assert_eq!(ok.latest_version, "2.0.0");
        assert_eq!(ok.last_check, 200);
        let err = apply_check_outcome(&ok, None, 300);
        assert_eq!(err.latest_version, "2.0.0", "失败不清掉已知版本");
        assert_eq!(err.last_check, 300);
    }

    /// dismiss 无可用版本时 no-op（不误写 dismissed_version）。
    #[test]
    fn dismiss_without_available_version_is_noop() {
        let s = state("", "", true, 0);
        let (s2, applied) = apply_dismiss(&s);
        assert!(!applied);
        assert_eq!(s2, s);
        // latest <= current：同样 no-op
        let s = state("0.0.1", "", true, 0);
        let (_, applied) = apply_dismiss(&s);
        assert!(!applied);
    }

    /// version_gt：semver 与 fallback 两路（从 diagnostic 迁移后行为不变）。
    #[test]
    fn version_gt_semver_and_fallback() {
        assert!(version_gt("0.26.21", "0.26.20"));
        assert!(!version_gt("0.26.20", "0.26.20"));
        assert!(!version_gt("0.26.19", "0.26.20"));
        // 非标准（缺段）走 fallback
        assert!(version_gt("0.27", "0.26.20"));
        assert!(!version_gt("abc", "abc"));
    }

    /// 主窗口检查动作结果条目：三分支（新版/已最新/失败）× 双语，字段形状
    /// 对齐 AppEntry 序列化（icon 字段走前端通用图标分支）。
    #[test]
    fn check_result_items_three_branches_both_langs() {
        use crate::app::commands::UpdateCheckOutcome;

        let ok_latest = UpdateCheckOutcome {
            has_update: false,
            current_version: "0.26.20".into(),
            latest_version: "0.26.20".into(),
            release_url: String::new(),
            release_notes: String::new(),
            updates: Vec::new(),
            error: None,
            degraded: false,
        };
        let zh = check_result_items(&ok_latest, "zh");
        assert_eq!(zh.len(), 1);
        assert_eq!(zh[0]["name"], "已是最新版本（0.26.20）");
        assert_eq!(zh[0]["icon"], "check");
        assert_eq!(zh[0]["source"], "update");
        assert_eq!(zh[0]["actions"][0]["kind"], "copy");
        assert_eq!(zh[0]["actions"][0]["payload"], "0.26.20");
        let en = check_result_items(&ok_latest, "en");
        assert_eq!(en[0]["name"], "You're on the latest version (0.26.20)");

        let has_update = UpdateCheckOutcome {
            has_update: true,
            latest_version: "0.26.21".into(),
            ..ok_latest.clone()
        };
        let zh = check_result_items(&has_update, "zh");
        assert_eq!(zh[0]["name"], "发现新版本 0.26.21");
        assert_eq!(zh[0]["icon"], "download");
        assert_eq!(zh[0]["actions"][0]["kind"], "settings");
        assert_eq!(zh[0]["actions"][0]["payload"], "about");

        let failed = UpdateCheckOutcome {
            error: Some("网络请求失败".into()),
            ..ok_latest
        };
        let zh = check_result_items(&failed, "zh");
        assert_eq!(zh[0]["is_error"], true);
        assert_eq!(zh[0]["icon"], "triangle-alert");
        assert_eq!(zh[0]["description"], "网络请求失败");
        assert_eq!(
            zh[0]["actions"].as_array().map(Vec::len),
            Some(0),
            "错误行不带动作（前端 isError 拦截激活）"
        );
    }
}
