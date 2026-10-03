//! 自启动：系统应用并验证后保存偏好；失败补偿到原系统状态。
use crate::app::config::{AppearanceConfig, ConfigStore};
use std::future::Future;
use tauri::Manager;

static UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn system_state(app: tauri::AppHandle, value: Option<bool>) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_autostart::ManagerExt;
        let manager = app.autolaunch();
        if let Some(enabled) = value {
            if enabled {
                manager.enable()
            } else {
                manager.disable()
            }
            .map_err(|e| e.to_string())?;
        }
        manager.is_enabled().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 明确重置同样进入自启动串行与补偿流程，全量偏好一次事务写入。
pub async fn reset(
    app: &tauri::AppHandle,
    config: &crate::app::config::AppConfig,
) -> Result<(), String> {
    let _guard = UPDATE_LOCK.lock().await;
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let persist = || crate::app::config::save_config(pool, config);
    if cfg!(debug_assertions) {
        persist().await
    } else {
        apply_then_save(
            config.auto_start,
            |value| system_state(app.clone(), value),
            persist,
        )
        .await
    }
}

async fn apply_then_save<S, SF, P, PF>(desired: bool, system: S, persist: P) -> Result<(), String>
where
    S: Fn(Option<bool>) -> SF,
    SF: Future<Output = Result<bool, String>>,
    P: FnOnce() -> PF,
    PF: Future<Output = Result<(), String>>,
{
    let original = system(None)
        .await
        .map_err(|e| format!("autostart_failed: 无法读取系统自启动状态：{e}"))?;
    let applied = match system(Some(desired)).await {
        Ok(actual) if actual == desired => persist().await,
        Ok(_) => Err("autostart_failed: 系统自启动状态验证失败".into()),
        Err(e) => Err(format!("autostart_failed: 无法更新系统自启动：{e}")),
    };
    if let Err(error) = applied {
        tracing::warn!(%error, original, desired, "自启动更新失败，尝试恢复原系统状态");
        match system(Some(original)).await {
            Ok(actual) if actual == original => return Err(error),
            result => {
                tracing::error!(%error, ?result, original, "自启动补偿失败");
                return Err(format!(
                    "autostart_restore_failed: {error}；恢复原系统状态失败，请检查系统自启动设置后重试"
                ));
            }
        }
    }
    Ok(())
}

pub async fn update(
    app: &tauri::AppHandle,
    desired: bool,
    expected: Option<bool>,
) -> Result<(), String> {
    let _guard = UPDATE_LOCK.lock().await;
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let baseline = ConfigStore::get_checked::<AppearanceConfig>(pool).await?;
    if expected.is_some_and(|old| old != baseline.auto_start) {
        return Err("config_conflict: 自启动设置已被其他窗口修改，请重新读取后重试".into());
    }
    let persist = || async {
        ConfigStore::update::<AppearanceConfig>(pool, |cfg| {
            if cfg.auto_start != baseline.auto_start {
                return Err("config_conflict: 自启动设置已变化，请重新读取后重试".into());
            }
            cfg.auto_start = desired;
            Ok(())
        })
        .await
        .map(|_| ())
    };
    if cfg!(debug_assertions) {
        persist().await?;
    } else {
        let system = |value| system_state(app.clone(), value);
        apply_then_save(desired, system, persist).await?;
    }
    crate::app::setting_service::emit_changed(app, "app.appearance");
    tracing::info!(
        desired,
        system_registration = !cfg!(debug_assertions),
        "自启动设置已确认"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn system_is_verified_before_persistence() {
        let state = Arc::new(Mutex::new(false));
        let system = |value| {
            let state = state.clone();
            async move {
                let mut state = state.lock().unwrap();
                if let Some(value) = value {
                    *state = value;
                }
                Ok(*state)
            }
        };
        let saved = Arc::new(Mutex::new(false));
        apply_then_save(true, system, || async {
            assert!(*state.lock().unwrap(), "系统效果必须先于持久化");
            *saved.lock().unwrap() = true;
            Ok(())
        })
        .await
        .unwrap();
        assert!(*saved.lock().unwrap());
    }

    #[tokio::test]
    async fn wrong_system_result_does_not_persist() {
        let saved = Arc::new(Mutex::new(false));
        let error = apply_then_save(
            true,
            |_| async { Ok(false) },
            || async {
                *saved.lock().unwrap() = true;
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("验证失败"));
        assert!(!*saved.lock().unwrap());
    }

    #[tokio::test]
    async fn persistence_failure_restores_original_system_state() {
        let state = Arc::new(Mutex::new(false));
        let system = |value| {
            let state = state.clone();
            async move {
                let mut state = state.lock().unwrap();
                if let Some(v) = value {
                    *state = v;
                }
                Ok(*state)
            }
        };
        let error = apply_then_save(true, system, || async { Err("database failure".into()) })
            .await
            .unwrap_err();
        assert_eq!(error, "database failure");
        assert!(!*state.lock().unwrap());
    }

    #[tokio::test]
    async fn system_failure_does_not_persist() {
        let saved = Arc::new(Mutex::new(false));
        let system = |value| async move {
            if value == Some(true) {
                Err("denied".into())
            } else {
                Ok(false)
            }
        };
        let error = apply_then_save(true, system, || async {
            *saved.lock().unwrap() = true;
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(error.starts_with("autostart_failed:"));
        assert!(!*saved.lock().unwrap());
    }

    #[tokio::test]
    async fn failed_compensation_is_reported_explicitly() {
        let system = |value| async move {
            match value {
                None => Ok(false),
                Some(true) => Ok(true),
                Some(false) => Err("restore denied".into()),
            }
        };
        let error = apply_then_save(true, system, || async { Err("database failure".into()) })
            .await
            .unwrap_err();
        assert!(error.starts_with("autostart_restore_failed:"));
    }
}
